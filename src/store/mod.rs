//! Index registry: one BoostCore index per OpenSearch index, plus its mapping.

use anyhow::{Result, anyhow};
use boostcore::directory::MmapDirectory;
use boostcore::schema::*;
use boostcore::{Index, IndexReader, IndexWriter, TantivyDocument};
use parking_lot::RwLock;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::hash::BuildHasherDefault;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

mod names;
pub use names::*;

mod coerce;
pub use coerce::*;
mod dates;
pub use dates::*;
mod derive;
pub use derive::*;
mod ids;
mod mapping;
mod net;
pub use net::*;
mod objects;
mod registry;
mod settings;
mod translog;
mod writer;

/// Field roles in the fixed schema shared by every index.
#[derive(Clone, Copy)]
pub struct Fields {
    pub id: Field,
    pub source: Field,
    /// analysed JSON view -- backs `text` fields and numerics
    pub dynamic: Field,
    /// raw (untokenised) JSON view -- backs `keyword` fields, sorts and term aggs
    pub raw: Field,
    /// the order the write arrived in, which is what `_seq_no` reports and
    /// what settles ties between equally-ranked documents
    pub seq: Field,
}

/// How much un-refreshed document source may sit in memory before the writer
/// flushes. Without a cap, a large bulk load holds every document twice.
pub const PENDING_BUDGET_BYTES: usize = 32 * 1024 * 1024;

/// Where an index keeps the writes that are acknowledged but not yet committed.
pub const TRANSLOG: &str = "translog.ndjson";

/// How large that record may grow before the index is committed to spend it.
/// OpenSearch calls this `index.translog.flush_threshold_size`.
const TRANSLOG_FLUSH_BYTES: u64 = 64 * 1024 * 1024;

/// How many writes wait for their shard's refresh before they are handed to
/// the writer anyway.
///
/// The queue is what lets one shard's refresh show one shard's writes, and it
/// is only worth keeping while a refresh is close behind. A load bigger than
/// this is past that: it goes to the writer, which is not the same as showing
/// it -- that still takes a commit and a reload.
const DEFERRED_MAX_OPS: usize = 2048;

/// Value-kind bits recorded per field path.
pub const KIND_I64: u8 = 1;
pub const KIND_U64: u8 = 2;
pub const KIND_F64: u8 = 4;
pub const KIND_STR: u8 = 8;
pub const KIND_BOOL: u8 = 16;
/// A string that parses as a date: BoostCore indexes it as a date, not as text,
/// so a range over it must address the date column and not the string one.
pub const KIND_DATE: u8 = 32;

/// Ids are already hashed into 64 bits before they reach the set, so the set
/// itself does not need to hash again.
#[derive(Default)]
pub struct IdHasher(u64);

impl std::hash::Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 ^ *b as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = v;
    }
}

pub const DYN: &str = "_dyn";
pub const RAW: &str = "_raw";

pub fn build_schema() -> (Schema, Fields) {
    let mut sb = Schema::builder();
    let id = sb.add_text_field("_id", STRING | STORED | FAST);
    let source = sb.add_text_field("_source", STORED);
    let dynamic = sb.add_json_field(
        DYN,
        JsonObjectOptions::default().set_fast(None).set_expand_dots_enabled().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("default")
                .set_fieldnorms(true)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        ),
    );
    // `_raw` keeps its own fast fields: BoostCore's RangeQuery over a JSON field
    // only works on fast fields, so dropping them here breaks every range query
    // that resolves to the untokenised view. Measured: removing them buys ~5% of
    // the write path, which is not worth the semantics.
    let raw = sb.add_json_field(
        RAW,
        JsonObjectOptions::default()
            .set_fast(Some("raw"))
            .set_expand_dots_enabled()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("raw")
                    .set_index_option(IndexRecordOption::Basic),
            ),
    );
    // The writer spreads one bulk request across its worker threads, so a
    // document's segment and doc id do not follow the order it was sent in.
    // Recording that order is what lets two equally-scored hits come back the
    // same way twice.
    let seq = sb.add_u64_field("_seq", FAST);
    (sb.build(), Fields { id, source, dynamic, raw, seq })
}

/// Declared field types, flattened to dotted paths (`user.name` -> `keyword`).
#[derive(Default, Clone, Debug)]
pub struct Mapping {
    pub types: HashMap<String, String>,
    /// the mapping body exactly as the user sent it, for GET _mapping
    pub raw: Value,
    /// The multi-fields with a normalizer, worked out once when the mapping
    /// changes: every document would otherwise walk the whole mapping looking
    /// for them, and most mappings have none.
    /// (parent, sub, normalizer, JSON pointer to the parent, full sub path)
    subs: Vec<(String, String, String, String, String)>,
    /// A field declared as an `alias` and the field it stands for. Asking the
    /// mapping what type a path holds is done once per node of every document
    /// written, and reading that out of the mapping tree -- a formatted string
    /// and a walk -- was the single most expensive thing about indexing a
    /// mapped document.
    aliases: HashMap<String, String>,
    /// The `format` a date path declares, for the same reason.
    formats: HashMap<String, String>,
    /// The fields whose values are written into other fields as well, worked
    /// out once: every document would otherwise walk the mapping for them.
    copies: Vec<(String, Vec<String>)>,
    /// The objects told to hold no objects of their own.
    flat_objects: std::collections::HashSet<String>,
    /// Whether any field holds queries, which is what decides if a document
    /// is checked for what its queries would fail on.
    has_percolator: bool,
    /// The fields of a few kinds every document is looked over for, listed
    /// once rather than found among all the types on every write.
    ranges: Vec<(String, String)>,
    flats: Vec<String>,
    shingled: Vec<String>,
    nanos: Vec<String>,
    /// the fields a script makes from the source, as (name, definition)
    derived: Vec<(String, Value)>,
}

impl Mapping {}

/// Record the value kinds present under each path.
///
/// Runs on every document, so it reuses one path buffer and only allocates when
/// a path is seen for the first time.
fn observe_kinds(v: &Value, path: &mut String, out: &mut HashMap<String, u8>) {
    match v {
        Value::Object(o) => {
            let base = path.len();
            for (k, child) in o {
                if base > 0 {
                    path.push('.');
                }
                path.push_str(k);
                observe_kinds(child, path, out);
                path.truncate(base);
            }
        }
        Value::Array(a) => {
            for x in a {
                observe_kinds(x, path, out);
            }
        }
        leaf if !path.is_empty() => {
            let bit = match leaf {
                Value::String(s) => {
                    if crate::query::parse_datetime(s).is_some() {
                        KIND_DATE
                    } else {
                        KIND_STR
                    }
                }
                Value::Bool(_) => KIND_BOOL,
                Value::Number(n) => {
                    if n.is_f64() && n.as_i64().is_none() && n.as_u64().is_none() {
                        KIND_F64
                    } else if n.as_i64().is_some() {
                        KIND_I64
                    } else {
                        KIND_U64
                    }
                }
                _ => return,
            };
            match out.get_mut(path.as_str()) {
                Some(seen) => *seen |= bit,
                None => {
                    out.insert(path.clone(), bit);
                }
            }
        }
        _ => {}
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DocMeta {
    pub version: u64,
    pub live: bool,
}

/// A write waiting for the shard it belongs to to be refreshed.
///
/// A refresh in OpenSearch reaches one shard: a delete on the shard holding
/// document 1 becomes visible while a delete on another shard does not. One
/// BoostCore index stands in for every shard here, and a commit would show
/// everything at once -- so an operation waits here until its own shard is
/// refreshed, and only then reaches the writer.
pub enum PendingOp {
    Add(Box<TantivyDocument>),
    Delete(String),
}

pub struct IdxState {
    pub name: String,
    /// whether this index was last brought back from a snapshot
    pub restored: bool,
    pub index: Index,
    /// Created on first write. An index that is only read -- or has not been
    /// written to since startup -- should not hold indexing threads or an arena.
    writer: Option<IndexWriter>,
    writer_threads: usize,
    writer_budget: usize,
    /// When this index was last written to. A writer holds indexing threads and
    /// an arena, so an index that has gone quiet should not keep one.
    last_write: std::time::Instant,
    pub reader: IndexReader,
    pub fields: Fields,
    pub mapping: Mapping,
    pub settings: Value,
    /// The analyzers this index's settings define, on top of the built-ins.
    pub analysis: crate::analysis::Registry,
    /// alias name -> its definition (filter, routing, is_write_index)
    pub aliases: HashMap<String, Value>,
    /// closed indices reject reads and writes until reopened
    pub closed: bool,
    /// Exact record for ids that need one: anything updated past version 1, and
    /// every tombstone. In an append-only workload this stays empty.
    pub versions: HashMap<String, DocMeta>,
    /// the routing a document was written with, kept only for the documents
    /// that were given one -- which is the rare case
    pub routing: HashMap<String, String>,
    /// a stable identifier for the index itself, distinct from the id of any
    /// one commit; 22 characters, as the API reports them
    pub uuid: String,
    /// when the index was made, in milliseconds since the epoch
    pub created_ms: u64,
    /// 64-bit fingerprints of ids believed live. A miss is authoritative (no
    /// false negatives), so the common "is this a new document?" question costs
    /// one hash. A hit is confirmed against the index, which only happens for
    /// ids that really were written before.
    pub live_ids: std::collections::HashSet<u64, BuildHasherDefault<IdHasher>>,
    /// Writes not yet visible to search -- `Some(json)` = upsert, `None` =
    /// tombstone. Kept as raw JSON to avoid holding a parsed tree per document.
    pub pending: HashMap<String, Option<String>>,
    /// Writes the writer has not been handed yet, by the shard they belong to.
    deferred: Vec<(u64, PendingOp)>,
    /// arrival order of the writes not yet visible to the refreshed reader
    pub pending_seq: HashMap<String, u64>,
    pub pending_bytes: usize,
    /// A second reader that IS advanced when the buffer is flushed, so GET stays
    /// realtime while search still only moves on an explicit refresh.
    pub realtime: IndexReader,
    pub seq_no: u64,
    /// number of searches served, reported by _stats. Atomic so counting a
    /// search never needs a write lock -- taking one here would deadlock any
    /// caller that already holds the read guard.
    pub search_count: std::sync::atomic::AtomicU64,
    /// misses recorded for `request_cache=true` searches, reported by _stats
    pub request_cache_miss: std::sync::atomic::AtomicU64,
    /// per-group query counts, from the `stats` field of a search body
    pub search_groups: RwLock<HashMap<String, u64>>,
    /// Fields whose ordinals have been read into memory: sorting on a field
    /// or aggregating over its ordinals loads them, and that is what the
    /// fielddata statistic reports on.
    pub loaded_fielddata: RwLock<std::collections::HashSet<String>>,
    pub auto_id: u64,
    /// field paths seen in indexed documents, with the type OpenSearch's
    /// dynamic mapping would have given them. Explicit mappings win over these.
    pub dynamic_types: HashMap<String, String>,
    /// hashes of document shapes already folded into `dynamic_types`
    pub seen_shapes: std::collections::HashSet<u64>,
    /// Which value kinds each field path has actually held. Lets a range query
    /// skip the typed variants that cannot possibly match anything.
    pub observed_kinds: HashMap<String, u8>,
    /// True only when `observed_kinds` covers every document in the index. An
    /// index written before kinds were tracked has partial information, and
    /// narrowing a range with it would silently drop matches.
    pub kinds_complete: bool,
    /// Whether any document here carries an explicit `_doc_count`.
    pub has_doc_count: bool,
    /// Updates that changed nothing, which the stats report separately.
    pub noop_updates: std::sync::atomic::AtomicU64,
    /// how many times this index has been flushed, which `_stats` reports
    pub flushes: std::sync::atomic::AtomicU64,
    /// how many documents have been fetched by id, which is what `_stats`
    /// counts under `get` -- a terms lookup fetches one too
    pub gets: std::sync::atomic::AtomicU64,
    /// how many bytes of document the index has been given, which is the size
    /// a rollover condition asks about
    pub bytes: std::sync::atomic::AtomicU64,
    kind_path_buf: String,
    /// where this index lives on disk, if it is persisted
    pub path: Option<PathBuf>,
    /// how much has been recorded since the last commit spent the record
    translog_bytes_since_commit: u64,
    /// Writes recorded where a crash can still find them.
    ///
    /// A write is in the index only once the writer has committed, and a
    /// commit is expensive enough that it cannot happen per request. Until it
    /// does, the only record of an acknowledged write is this file -- which is
    /// what `index.translog.durability: request` means: appended and fsynced
    /// before the write is answered.
    translog: Option<std::io::BufWriter<std::fs::File>>,
    /// per-segment block statistics, built on demand
    pub stats: Arc<crate::blockstats::StatsCache>,
    /// False while the id table is still being rebuilt after a reopen. Until it
    /// flips, an unknown id has to be checked against the index itself.
    pub ids_loaded: Arc<std::sync::atomic::AtomicBool>,
}

impl IdxState {
    /// Tell the index what its settings and mapping say about analysis.
    ///
    /// The analyzers an index defines are registered under the names the
    /// mapping uses, and every path that names one is recorded, so that
    /// BoostCore cuts that path with it and leaves the rest alone. Called
    /// whenever either of the two can have changed.
    pub fn apply_analysis(&mut self) {
        self.analysis = crate::analysis::Registry::from_settings(&self.settings);
        for name in self.analysis.names() {
            if let Some(chain) = self.analysis.get(&name) {
                self.index.tokenizers().register(&name, chain.analyzer());
            }
        }
        let paths = self.index.path_analyzers().clone();
        paths.clear_field(DYN);
        for (path, analyzer) in self.mapping.analyzed_paths() {
            // a name the index never defined may still be one of the analyzers
            // OpenSearch has without being told about them
            let Some(chain) = self.analysis.get(&analyzer) else { continue };
            self.index.tokenizers().register(&analyzer, chain.analyzer());
            paths.set(DYN, &path, &analyzer);
        }
    }
}

#[derive(Clone)]
pub struct Store {
    inner: Arc<RwLock<HashMap<String, Arc<RwLock<IdxState>>>>>,
    /// where index data lives; `None` keeps everything in RAM
    data_dir: Option<PathBuf>,
    /// index templates by name
    templates: Arc<RwLock<HashMap<String, Value>>>,
    /// live scroll cursors, keyed by the id handed to the client
    scrolls: Arc<RwLock<HashMap<String, ScrollState>>>,
    /// What a walk over a query answered, for the caller that asked not to
    /// wait for it. Nothing here runs long enough to need waiting on, so the
    /// answer is ready before the task's name is handed out.
    tasks: Arc<RwLock<HashMap<String, Value>>>,
    task_seq: Arc<std::sync::atomic::AtomicU64>,
    /// the scripts and templates stored under a name
    scripts: Arc<RwLock<HashMap<String, Value>>>,
    scroll_seq: Arc<std::sync::atomic::AtomicU64>,
    /// One search thread pool for the whole process. Giving each index its own
    /// costs a pool per index, which is invisible with one index and ruinous
    /// with hundreds.
    executor: boostcore::Executor,
    /// Indices holding a live writer, oldest first, capped so a load touching
    /// hundreds of indices cannot hold hundreds of sets of indexing threads.
    ///
    /// Measured: capping this does *not* reduce the memory retained after a
    /// write burst (11.15 MB/index uncapped vs 11.37 MB/index at a cap of 8).
    /// It is kept for the thread bound, not as a memory fix.
    live_writers: Arc<RwLock<Vec<String>>>,
    /// cluster-level settings, which a few APIs read back and one or two enforce
    cluster_settings: Arc<RwLock<Value>>,
    /// nodes excluded from the voting configuration, which this engine records
    /// and reports without having a vote to hold
    voting_exclusions: Arc<RwLock<Vec<Value>>>,
    /// component templates: settings and mappings named once and composed
    /// into whichever index templates ask for them
    components: Arc<RwLock<HashMap<String, Value>>>,
    /// open points in time, each remembering where every index it covers had
    /// got to when it was opened
    pits: Arc<RwLock<HashMap<String, PitState>>>,
    /// Data streams by name, each remembering the template it was made from.
    data_streams: Arc<RwLock<HashMap<String, String>>>,
    /// Pipelines by kind ("ingest" or "search") and then by name.
    pipelines: Arc<RwLock<HashMap<String, HashMap<String, Value>>>>,
    /// how often each ingest pipeline ran, failed, and how long it took, in
    /// nanoseconds; the empty name is the total
    pub ingest_stats: Arc<RwLock<HashMap<String, (u64, u64, u64)>>>,
    /// the indices deleted since the node came up: name, uuid and when
    pub graveyard: Arc<RwLock<Vec<Value>>>,
    /// Snapshot repositories by name.
    repositories: Arc<RwLock<HashMap<String, Value>>>,
    /// Snapshots by repository and then by name.
    snapshots: Arc<RwLock<HashMap<String, HashMap<String, Value>>>>,
    pit_seq: Arc<std::sync::atomic::AtomicU64>,
}

impl Store {
    /// Merge one `_cluster/settings` body in, dropping the keys set to null.
    pub fn merge_cluster_settings(&self, body: &Value) {
        let mut g = self.cluster_settings.write();
        for scope in ["persistent", "transient"] {
            let Some(incoming) = body.get(scope).and_then(|v| v.as_object()) else { continue };
            let Some(dest) = g.get_mut(scope).and_then(|v| v.as_object_mut()) else { continue };
            for (k, v) in incoming {
                if v.is_null() {
                    dest.remove(k);
                } else {
                    dest.insert(k.clone(), v.clone());
                }
            }
        }
    }

    /// Open a point in time over an expression: what each index it reaches
    /// had written by now, so a later search can be held to that.
    pub fn open_pit(&self, expr: &str, keep_alive_ms: u64) -> String {
        let n = self.pit_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!("boostsearch-pit-{n:016x}");
        let mut ceiling = HashMap::new();
        for name in self.resolve(expr) {
            if let Some(st) = self.get(&name) {
                ceiling.insert(name, st.read().seq_no);
            }
        }
        self.pits
            .write()
            .insert(id.clone(), PitState { expr: expr.to_string(), ceiling, keep_alive_ms });
        id
    }

    pub fn read_pit(&self, id: &str) -> Option<PitState> {
        self.pits.read().get(id).cloned()
    }

    pub fn all_pits(&self) -> Vec<(String, PitState)> {
        self.pits.read().iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub fn close_pit(&self, id: &str) -> bool {
        self.pits.write().remove(id).is_some()
    }

    pub fn put_component(&self, name: &str, body: Value) {
        self.components.write().insert(name.to_string(), body);
    }

    pub fn get_components(&self) -> HashMap<String, Value> {
        self.components.read().clone()
    }

    pub fn delete_component(&self, name: &str) -> bool {
        self.components.write().remove(name).is_some()
    }

    pub fn add_voting_exclusions(&self, entries: Vec<Value>) {
        let mut g = self.voting_exclusions.write();
        for e in entries {
            if !g.contains(&e) {
                g.push(e);
            }
        }
    }

    pub fn clear_voting_exclusions(&self) {
        self.voting_exclusions.write().clear();
    }

    pub fn voting_exclusions(&self) -> Vec<Value> {
        self.voting_exclusions.read().clone()
    }

    pub fn cluster_settings(&self) -> Value {
        self.cluster_settings.read().clone()
    }

    /// A cluster setting by name; a transient value shadows a persistent one.
    pub fn cluster_setting(&self, key: &str) -> Option<Value> {
        let g = self.cluster_settings.read();
        for scope in ["transient", "persistent"] {
            if let Some(v) = g.get(scope).and_then(|s| s.get(key)) {
                return Some(v.clone());
            }
        }
        None
    }
}

/// Hand memory freed by a finished write burst back to the OS.
///
/// Indexing allocates and frees a great deal per index; glibc keeps those
/// chunks in its arenas, which is invisible with one index and looks like a
/// leak with hundreds. Everything here is already dropped -- this only returns
/// what is no longer referenced.
pub fn release_freed_memory() {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::malloc_trim(0);
    }
}

fn shared_executor() -> boostcore::Executor {
    let threads = std::env::var("BOOSTSEARCH_SEARCH_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4));
    if threads <= 1 {
        return boostcore::Executor::single_thread();
    }
    boostcore::Executor::multi_thread(threads, "boostsearch-search-")
        .unwrap_or_else(|_| boostcore::Executor::single_thread())
}

impl Store {}

/// A scroll is a cursor over a search: the request that opened it plus how far
/// the client has read.
/// A point in time: which indices it covers, and how far each had got.
#[derive(Clone)]
pub struct PitState {
    pub expr: String,
    /// per index, the sequence number the next write will take -- everything
    /// below it was already there when the point in time was opened
    pub ceiling: HashMap<String, u64>,
    pub keep_alive_ms: u64,
}

#[derive(Clone)]
pub struct ScrollState {
    pub expr: String,
    pub body: Value,
    pub offset: usize,
    pub size: usize,
    /// the point in time the scroll was opened over, so that documents
    /// written after it are not walked into
    pub pit: String,
}

fn walk_malformed(
    node: &Value,
    path: &mut String,
    mapping: &Mapping,
    index_default: bool,
    ignored: &mut Vec<String>,
) -> std::result::Result<(), (String, String)> {
    match node {
        Value::Object(obj) => {
            let base = path.len();
            for (k, v) in obj {
                if base > 0 {
                    path.push('.');
                }
                path.push_str(k);
                let r = walk_malformed(v, path, mapping, index_default, ignored);
                path.truncate(base);
                r?;
            }
        }
        Value::Array(items) => {
            for v in items {
                walk_malformed(v, path, mapping, index_default, ignored)?;
            }
        }
        // a null is a document with no value for the field, not a bad one
        Value::Null => {}
        leaf => {
            let Some(ty) = mapping.type_of(path) else { return Ok(()) };
            // the format is read from what the mapping worked out once: this
            // runs for every leaf of every document written, and a walk of
            // the mapping tree per leaf was most of the cost of indexing
            let fmt = mapping.date_format(path);
            if value_is_valid(leaf, ty, fmt) {
                return Ok(());
            }
            let lenient = mapping
                .field_option(path, "ignore_malformed")
                .and_then(|v| match v {
                    Value::Bool(b) => Some(b),
                    Value::String(s) => s.parse().ok(),
                    _ => None,
                })
                .unwrap_or(index_default);
            if lenient {
                ignored.push(path.clone());
            } else {
                return Err((path.clone(), ty.to_string()));
            }
        }
    }
    Ok(())
}

/// The window a date column can hold. Nanoseconds in an i64 reach about 292
/// years either side of the epoch, so an open-ended range is filled to the
/// edges of that rather than to a year the column could not represent.
/// The open side of a date range, as the number the index holds: a date is
/// milliseconds here, so these are the ends of what a range can reach.
const DATE_FLOOR: i64 = -8_520_336_000_000;
const DATE_CEIL: i64 = 8_835_004_800_000;

/// Where a flat_object field's values are gathered so the field itself can be
/// queried without naming a path inside it.
pub const FLAT_VALUES: &str = "_bs_values";

/// How many tokens a standard analyser would find.
pub fn token_count(text: &str) -> u64 {
    text.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).count() as u64
}

fn format_millis_utc(ms: i64, format: &str) -> Option<String> {
    let dt =
        boostcore::time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000).ok()?;
    Some(match format {
        "epoch_millis" => ms.to_string(),
        "epoch_second" => (ms / 1000).to_string(),
        "strict_date" | "date" | "yyyy-MM-dd" => format_with_pattern(dt, "yyyy-MM-dd"),
        "basic_date" => format_with_pattern(dt, "yyyyMMdd"),
        "iso8601"
        | "strict_date_optional_time"
        | "date_optional_time"
        | "date_time"
        | "strict_date_time" => format!(
            "{}.{:03}Z",
            format_with_pattern(dt, "yyyy-MM-dd'T'HH:mm:ss").replace('\'', ""),
            dt.millisecond()
        ),
        "strict_date_hour_minute_second" | "date_hour_minute_second" => {
            format_with_pattern(dt, "yyyy-MM-dd'T'HH:mm:ss").replace('\'', "")
        }
        other => format_with_pattern(dt, other),
    })
}

fn days_in_month(year: i32, month: boostcore::time::Month) -> u8 {
    use boostcore::time::Month::*;
    match month {
        January | March | May | July | August | October | December => 31,
        April | June | September | November => 30,
        February => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

/// The value at `key` inside `node`, made by `make` if it is not there.
///
/// What is being built here is the index's own view of a mapping or a
/// settings tree, not a document a client sent, so a value that should be an
/// object and is not is replaced rather than complained about.
pub(crate) fn entry_of<'a>(
    node: &'a mut Value,
    key: &str,
    make: impl FnOnce() -> Value,
) -> &'a mut Value {
    if !node.is_object() {
        *node = serde_json::json!({});
    }
    node.as_object_mut()
        .expect("replaced with an object just above")
        .entry(key.to_string())
        .or_insert_with(make)
}
