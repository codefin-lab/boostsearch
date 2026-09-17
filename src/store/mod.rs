//! Index registry: one VeloCore index per OpenSearch index, plus its mapping.

use anyhow::{Result, anyhow};
use parking_lot::RwLock;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::hash::BuildHasherDefault;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use velocore::directory::MmapDirectory;
use velocore::schema::*;
use velocore::{Index, IndexReader, IndexWriter, TantivyDocument};

mod names;
pub use names::*;

mod coerce;
pub mod counters;
pub mod slowlog;
pub use coerce::*;
mod dates;
pub use dates::*;
mod derive;
pub use derive::*;
mod ids;
pub(crate) use ids::alive_address;
pub mod mapping;
mod net;
pub use net::*;
mod objects;
mod pit;
pub use pit::{PitId, PitPart, PitState};
mod registry;
mod settings;
mod translog;
mod writer;

/// Field roles in the fixed schema shared by every index.
#[derive(Clone, Copy)]
pub struct Fields {
    pub id: Field,
    pub source: Field,
    /// analysed JSON view -- backs `text` fields: the words, with their
    /// positions and the frequencies a score is worked out from
    pub dynamic: Field,
    /// untouched JSON view -- backs everything exact: keywords, numbers,
    /// dates, and every column a sort or an aggregation reads
    pub raw: Field,
    /// the analysed words again, with a column over them, for the text
    /// fields that declared `fielddata: true` and nothing else
    pub fielddata: Field,
    /// the order the write arrived in, which is what `_seq_no` reports and
    /// what settles ties between equally-ranked documents
    pub seq: Field,
}

/// The settings a write consults, cached off the settings tree.
#[derive(Clone, Debug, Default)]
pub struct WriteKnobs {
    pub blocks_write: bool,
    /// `index.blocks.read_only`: nothing may be written and the index may
    /// not be deleted
    pub blocks_read_only: bool,
    /// `index.blocks.read_only_allow_delete`: what `read_only` refuses,
    /// but as the flood-stage block a full disk puts on, answered 429
    pub blocks_read_only_allow_delete: bool,
    pub ignore_malformed: bool,
    pub append_only: bool,
    pub nested_limit: u64,
    pub durability: Option<String>,
    /// how long `durability: async` may leave a record unforced, as
    /// `index.translog.sync_interval` says
    pub sync_interval_ms: u64,
    /// how many shards the index has, which every write is placed by
    pub shards: u64,
    /// the slow log thresholds, which every search and write compares its
    /// time against
    pub slowlog: slowlog::SlowLogKnobs,
}

/// How much un-refreshed document source may sit in memory before the writer
/// flushes. Without a cap, a large bulk load holds every document twice.
pub const PENDING_BUDGET_BYTES: usize = 32 * 1024 * 1024;

/// Where an index keeps the writes that are acknowledged but not yet committed.
pub const TRANSLOG: &str = "translog.ndjson";

/// How long a search context nobody named a keep-alive for is kept, as
/// OpenSearch's `search.default_keep_alive` says: five minutes.
pub const DEFAULT_KEEP_ALIVE_MS: u64 = 5 * 60 * 1000;

/// How many scrolls may be open at once, as `search.max_open_scroll_context`
/// says: each holds a point in time, and a point in time holds segments open.
pub const MAX_OPEN_SCROLLS: usize = 500;

/// How long a context lives when it was asked for: what was asked, or the
/// default where nothing was.
pub(crate) fn keep_for(keep_alive_ms: u64) -> std::time::Duration {
    std::time::Duration::from_millis(match keep_alive_ms {
        0 => DEFAULT_KEEP_ALIVE_MS,
        asked => asked,
    })
}

/// The types a mapping body declares, as `path -> type` pairs.
///
/// Used to check that an update does not change a field's type; the walk is
/// over the body the caller sent, not over the mapping it would become.
pub fn declared_types(body: &Value) -> Vec<(String, String)> {
    fn walk(node: &Value, prefix: &str, out: &mut Vec<(String, String)>) {
        let Some(props) = node.get("properties").and_then(|p| p.as_object()) else { return };
        for (name, spec) in props {
            let path = match prefix.is_empty() {
                true => name.clone(),
                false => format!("{prefix}.{name}"),
            };
            if let Some(ty) = spec.get("type").and_then(|t| t.as_str()) {
                out.push((path.clone(), ty.to_string()));
            }
            walk(spec, &path, out);
        }
    }
    let mut out = Vec::new();
    walk(body, "", &mut out);
    out
}

/// A name nobody can guess: whoever holds a search context's id can read it,
/// so the id is drawn at random rather than counted up from zero.
pub(crate) fn random_token() -> String {
    crate::cluster::NodeId::random().as_str().to_string()
}

/// The caller a search context belongs to, where callers are told apart.
pub(crate) fn current_owner() -> Option<String> {
    crate::security::layer::current_caller().map(|c| c.name.clone())
}

/// Whether the caller now is the one a context was opened by. A context
/// opened when nobody was named is open to anyone, which is what a server
/// with security off means.
pub(crate) fn owner_matches(owner: &Option<String>) -> bool {
    match (owner, current_owner()) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(a), Some(b)) => *a == b,
    }
}

/// Write a file so that a crash finds either what was there before or what
/// is written here, and never half of either.
///
/// A file rewritten in place is torn by a crash between the truncate and the
/// last byte, and a torn `_meta.json` is an index that is not there on the
/// next start. The bytes go to a file beside it, are forced, and the rename
/// puts them in place -- and the directory is forced too, since the rename
/// is what has to survive.
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    // the temporary file is this writer's own: with one name for all of
    // them, a second writer truncated the first one's file and the rename
    // then failed or, worse, put half a file in place
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mark = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.{}.{mark}.tmp", std::process::id()));
    // whatever happens to this write, the temporary file does not outlive it
    struct Sweep<'a>(&'a std::path::Path);
    impl Drop for Sweep<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0);
        }
    }
    let _sweep = Sweep(&tmp);
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
    Ok(())
}

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
/// A string that parses as a date: VeloCore indexes it as a date, not as text,
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
/// The view that carries a column for a text field that asked for one.
pub const FIELDDATA: &str = "_fd";

/// Now, in milliseconds since the epoch -- which is the only clock anything
/// in an answer is measured against.
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Handed out so that no two indices, and no two lives of one index, ever
/// stand behind the same generation number.
pub(crate) fn next_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub fn build_schema() -> (Schema, Fields) {
    let mut sb = Schema::builder();
    // no field norms: an id is looked up, never scored, and a norm is a byte
    // per document per field that nothing reads
    let id_options = TextOptions::default().set_stored().set_fast(None).set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("raw")
            .set_fieldnorms(false)
            .set_index_option(IndexRecordOption::Basic),
    );
    let id = sb.add_text_field("_id", id_options);
    let source = sb.add_text_field("_source", STORED);
    let dynamic = sb.add_json_field(
        DYN,
        // No columns. A column is read to sort by a field or to aggregate over
        // it, and neither is done on analysed words: everything that has a
        // column has it on the untouched view. Keeping one here as well was a
        // second copy of every value in the index.
        JsonObjectOptions::default().set_expand_dots_enabled().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("default")
                .set_fieldnorms(true)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        ),
    );
    // `_raw` keeps its own fast fields: VeloCore's RangeQuery over a JSON field
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
                    // the untouched view is never scored by how long a field
                    // is -- that is what the analysed view is for -- and a
                    // keyword field carries no norms in OpenSearch either
                    .set_fieldnorms(false)
                    .set_index_option(IndexRecordOption::Basic),
            ),
    );
    // The writer spreads one bulk request across its worker threads, so a
    // document's segment and doc id do not follow the order it was sent in.
    // Recording that order is what lets two equally-scored hits come back the
    // same way twice.
    // The column a `fielddata: true` text field is sorted and aggregated by.
    // OpenSearch makes that opt-in because holding a text field's terms in
    // memory is expensive, and this is the same bargain: a mapping that never
    // asks for it never writes a byte here.
    let fielddata = sb.add_json_field(
        FIELDDATA,
        JsonObjectOptions::default().set_fast(None).set_expand_dots_enabled().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("default")
                .set_fieldnorms(false)
                .set_index_option(IndexRecordOption::Basic),
        ),
    );
    let seq = sb.add_u64_field("_seq", FAST);
    (sb.build(), Fields { id, source, dynamic, raw, fielddata, seq })
}

/// Declared field types, flattened to dotted paths (`user.name` -> `keyword`).
#[derive(Default, Clone, Debug)]
pub struct Mapping {
    pub types: HashMap<String, String>,
    /// the `knn_vector` fields declared here, worked out when the mapping
    /// changes rather than once per document written
    pub vector_fields: HashMap<String, crate::knn::Field>,
    /// Which of the two views each declared field's values are written into,
    /// worked out when the mapping changes rather than per document.
    pub views: HashMap<String, crate::store::mapping::Views>,
    /// whether every declared field goes to both views, in which case a
    /// document does not need splitting at all
    pub every_field_both: bool,
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
    /// the fields that say whether a malformed value is dropped or refused,
    /// read once rather than out of the mapping tree per leaf per write
    lenient: HashMap<String, bool>,
    /// the top-level key sets already found to be wholly mapped, so a
    /// document of a shape seen before is not walked for new fields
    mapped_shapes: std::collections::HashSet<u64>,
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

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct DocMeta {
    pub version: u64,
    pub live: bool,
}

/// A write waiting for the shard it belongs to to be refreshed.
///
/// A refresh in OpenSearch reaches one shard: a delete on the shard holding
/// document 1 becomes visible while a delete on another shard does not. One
/// VeloCore index stands in for every shard here, and a commit would show
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
    /// When this index was last refreshed, for the scheduled refresh.
    pub(crate) last_refresh: std::time::Instant,
    /// When this index was last searched, in milliseconds of the process
    /// clock: an index nobody searches is search-idle, and its scheduled
    /// refresh waits for the next search.
    pub(crate) last_search: std::sync::atomic::AtomicU64,
    /// the settings every write asks about, read once when they change
    /// rather than out of the settings tree for every document
    pub knobs: WriteKnobs,
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
    /// The primary term each document was last written in, for the
    /// documents whose term is not the first.
    ///
    /// `_primary_term` was answered as 1 for every document, and an
    /// `if_primary_term` other than 1 was refused -- true until the first
    /// failover and false after it: a write answered `_primary_term: 2`, a
    /// read of the same document answered 1, and a write conditioned on what
    /// the first write itself had said was refused as a conflict. The term is
    /// what tells two writes under one sequence number apart once a primary
    /// has changed, so it is kept per document. A shard that never fails over
    /// keeps nothing here.
    pub terms: HashMap<String, u64>,
    /// The documents whose version, liveness or term has moved since the
    /// versions and terms were last written down in full. Written out as a
    /// small record before the translog is thrown away -- see `doc_meta_log`.
    pub meta_dirty: std::collections::HashSet<String>,
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
    /// which queue each document's work is waiting in, so that a change of
    /// routing does not split one document's operations across two of them
    queued_shard: HashMap<String, u64>,
    /// arrival order of the writes not yet visible to the refreshed reader
    pub pending_seq: HashMap<String, u64>,
    pub pending_bytes: usize,
    /// A second reader that IS advanced when the buffer is flushed, so GET stays
    /// realtime while search still only moves on an explicit refresh.
    pub realtime: IndexReader,
    pub seq_no: u64,
    /// the highest primary term whose writes this copy has applied: a write
    /// from a newer primary wins whatever version stands here, since a
    /// promoted copy counts versions from what it holds, which may be behind
    pub applied_term: u64,
    /// What the index has been asked to do -- writes, reads, searches,
    /// refreshes, merges -- and how long it took, reported by `_stats`.
    /// Atomic, so counting never needs a write lock: taking one here would
    /// deadlock any caller that already holds the read guard.
    pub counters: counters::Counters,
    /// The vectors this index holds, which live beside the inverted index
    /// rather than in it: a term dictionary cannot answer "which documents
    /// are near this point".
    pub vectors: RwLock<crate::knn::Vectors>,
    /// searches answered out of the request cache, and searches that could
    /// have been but were not there yet, reported by _stats
    pub request_cache_hit: std::sync::atomic::AtomicU64,
    pub request_cache_miss: std::sync::atomic::AtomicU64,
    /// Which state of this index a cached answer belongs to. Every write,
    /// every refresh and every change to what the index is moves it on, so a
    /// remembered answer from before the change can no longer be found under
    /// the key that would be built now.
    ///
    /// It counts from a number no index has had before rather than from zero:
    /// an index deleted and made again under the same name would otherwise
    /// start where the old one started, and inherit answers about documents
    /// that are no longer there.
    pub search_gen: std::sync::atomic::AtomicU64,
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
    /// how many bytes of document the index has been given, which is the size
    /// a rollover condition asks about
    pub bytes: std::sync::atomic::AtomicU64,
    kind_path_buf: String,
    /// where this index lives on disk, if it is persisted
    pub path: Option<PathBuf>,
    /// the allocation id the cluster manager gave this copy of the index,
    /// which is how a returning node proves its copy is one that was in sync
    pub allocation_id: Option<String>,
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
    /// the first thing that went wrong recording writes since the last time a
    /// write was answered for; the write that finds it is not acknowledged
    pub(crate) translog_error: Option<String>,
    /// when the record was last forced to disk, which is what
    /// `durability: async` measures its interval from
    last_translog_sync: std::time::Instant,
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
    /// VeloCore cuts that path with it and leaves the rest alone. Called
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

/// What a pipeline has done: how many documents went through it, how many
/// failed, and how long it took in nanoseconds.
pub type IngestTally = (u64, u64, u64);

#[derive(Clone)]
pub struct Store {
    inner: Arc<RwLock<HashMap<String, Arc<IdxLock>>>>,
    /// where index data lives; `None` keeps everything in RAM
    data_dir: Option<PathBuf>,
    /// index templates by name
    templates: Arc<RwLock<HashMap<String, Value>>>,
    /// live scroll cursors, keyed by the id handed to the client
    scrolls: Arc<RwLock<HashMap<String, ScrollState>>>,
    /// the scripts and templates stored under a name
    scripts: Arc<RwLock<HashMap<String, Value>>>,
    /// One search thread pool for the whole process. Giving each index its own
    /// costs a pool per index, which is invisible with one index and ruinous
    /// with hundreds.
    executor: velocore::Executor,
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
    /// this node's parts of the open points in time, by the token their id
    /// carries, each holding the readers it was opened over
    pits: Arc<RwLock<HashMap<String, PitState>>>,
    /// What a search over an unchanged index already answered.
    pub request_cache: Arc<crate::search::RequestCache>,
    /// Data streams by name, each remembering the template it was made from.
    data_streams: Arc<RwLock<HashMap<String, String>>>,
    /// Pipelines by kind ("ingest" or "search") and then by name.
    pipelines: Arc<RwLock<HashMap<String, HashMap<String, Value>>>>,
    /// how often each ingest pipeline ran, failed, and how long it took, in
    /// nanoseconds; the empty name is the total
    pub ingest_stats: Arc<RwLock<HashMap<String, IngestTally>>>,
    /// the indices deleted since the node came up: name, uuid and when
    pub graveyard: Arc<RwLock<Vec<Value>>>,
    /// whether any ingest pipeline exists at all: while none does, no write
    /// needs to ask which pipelines apply to it
    pub any_ingest_pipeline: Arc<std::sync::atomic::AtomicBool>,
    /// Snapshot repositories by name.
    repositories: Arc<RwLock<HashMap<String, Value>>>,
    /// Snapshots by repository and then by name.
    snapshots: Arc<RwLock<HashMap<String, HashMap<String, Value>>>>,
    /// who may do what, and whether that is being asked at all
    pub security: Arc<crate::security::Security>,
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

    /// Search contexts nobody came back for: dropped when the next one is
    /// opened, which is when their memory is wanted.
    pub fn sweep_contexts(&self) {
        let now = std::time::Instant::now();
        self.scrolls.write().retain(|_, s| s.expires_at > now);
        self.pits.write().retain(|_, p| p.expires_at > now);
    }

    /// How many scrolls are open, which is what the ceiling counts.
    pub fn open_scrolls(&self) -> usize {
        self.scrolls.read().len()
    }

    pub fn put_component(&self, name: &str, body: Value) {
        self.components.write().insert(name.to_string(), body);
    }

    pub fn get_components(&self) -> HashMap<String, Value> {
        self.components.read().clone()
    }

    /// Every component the name or pattern names, deleted; whether there
    /// was one. A pattern was looked up as a name, so `DELETE
    /// _component_template/logs-*` deleted nothing and said so.
    pub fn delete_component(&self, name: &str) -> bool {
        let mut all = self.components.write();
        let named: Vec<String> = all
            .keys()
            .filter(|k| k.as_str() == name || wildcard_to_regex(name).is_match(k))
            .cloned()
            .collect();
        for n in &named {
            all.remove(n);
        }
        !named.is_empty()
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

fn shared_executor() -> velocore::Executor {
    let threads = std::env::var("VELOSEARCH_SEARCH_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4));
    if threads <= 1 {
        return velocore::Executor::single_thread();
    }
    velocore::Executor::multi_thread(threads, "velosearch-search-")
        .unwrap_or_else(|_| velocore::Executor::single_thread())
}

impl Store {}

/// A scroll is a cursor over a search: the request that opened it plus how far
/// the client has read.
#[derive(Clone)]
pub struct ScrollState {
    pub expr: String,
    /// the caller who opened it: a search context is theirs to read, and a
    /// scroll id is not a capability anyone who guesses it may spend
    pub owner: Option<String>,
    /// when it may be swept away, moved along by every batch
    pub expires_at: std::time::Instant,
    pub body: Value,
    pub offset: usize,
    pub size: usize,
    /// the point in time the scroll was opened over, so that documents
    /// written after it are not walked into
    pub pit: String,
    /// where the last batch ended, as the sort values of its last document.
    /// A scroll carried on by counting from the beginning costs more with
    /// every batch; carried on from here it costs the same each time.
    pub after: Option<Vec<Value>>,
    /// whether the order the scroll walks in is one it chose for itself, in
    /// which case the sort values do not belong in the answer
    pub implicit_sort: bool,
}

/// A value the mapping will not take, and what to say about it.
pub struct Malformed {
    pub field: String,
    pub ty: String,
    /// the value as the reference previews it in the message
    pub preview: String,
    /// the exception underneath, where there is one to name
    pub cause: Option<(String, String)>,
}

/// A value as the reference shows it in a parse complaint: a string without
/// its quotes, anything else as it was written.
pub fn preview_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn walk_malformed(
    node: &Value,
    path: &mut String,
    mapping: &Mapping,
    index_default: bool,
    ignored: &mut Vec<String>,
) -> std::result::Result<(), Malformed> {
    match node {
        Value::Object(obj) => {
            // a field the mapping declares as a plain value cannot hold an
            // object: OpenSearch refuses the document rather than writing
            // something the field could never be searched by
            if let Some(ty) = mapping.type_of(path)
                && !type_takes_an_object(ty)
            {
                let lenient = mapping.lenient_of(path).unwrap_or(index_default);
                if lenient {
                    ignored.push(path.clone());
                    return Ok(());
                }
                return Err(Malformed {
                    field: path.clone(),
                    ty: ty.to_string(),
                    preview: preview_of(node),
                    cause: None,
                });
            }
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
            let lenient = mapping.lenient_of(path).unwrap_or(index_default);
            if lenient {
                ignored.push(path.clone());
            } else {
                return Err(Malformed {
                    field: path.clone(),
                    ty: ty.to_string(),
                    preview: preview_of(leaf),
                    cause: coerce::out_of_range_cause(leaf, ty).or_else(|| {
                        leaf.as_str().map(|s| {
                            (
                                "number_format_exception".to_string(),
                                format!("For input string: \"{s}\""),
                            )
                        })
                    }),
                });
            }
        }
    }
    Ok(())
}

/// Can a field of this type hold an object rather than a plain value?
///
/// The containers can, and so can the handful of types written as objects:
/// a range with its ends, a point with its coordinates, a suggestion with its
/// weight, a stored query.
fn type_takes_an_object(ty: &str) -> bool {
    ty.ends_with("_range")
        || matches!(
            ty,
            "object"
                | "nested"
                | "flat_object"
                | "join"
                | "geo_point"
                | "geo_shape"
                | "xy_point"
                | "xy_shape"
                | "point"
                | "shape"
                | "percolator"
                | "completion"
                | "knn_vector"
                | "rank_features"
                | "aggregate_metric_double"
        )
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
pub const FLAT_VALUES: &str = "_vs_values";

/// How many tokens a standard analyser would find.
pub fn token_count(text: &str) -> u64 {
    text.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).count() as u64
}

fn format_millis_utc(ms: i64, format: &str) -> Option<String> {
    let dt =
        velocore::time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000).ok()?;
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

fn days_in_month(year: i32, month: velocore::time::Month) -> u8 {
    use velocore::time::Month::*;
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

/// The lock over one index.
///
/// A read taken by a thread that already holds one is granted even while a
/// writer waits. parking_lot's plain `read` queues behind a waiting writer,
/// and a search that read the index twice -- once for the shard, once for its
/// aliases -- waited on a bulk write that was waiting on the search: the
/// node's runtime threads filled with such waits and it stopped answering,
/// which is how one node of the chaos test fell silent while its cluster
/// thread went on committing.
pub struct IdxLock<T = IdxState>(RwLock<T>);

impl<T> IdxLock<T> {
    pub fn new(value: T) -> Self {
        IdxLock(RwLock::new(value))
    }

    pub fn read(&self) -> parking_lot::RwLockReadGuard<'_, T> {
        self.0.read_recursive()
    }

    pub fn write(&self) -> parking_lot::RwLockWriteGuard<'_, T> {
        self.0.write()
    }

    /// The write lock, if it can be had within `wait`: a node stopping does
    /// not wait behind a merge that may take minutes.
    pub fn try_write_for(
        &self,
        wait: std::time::Duration,
    ) -> Option<parking_lot::RwLockWriteGuard<'_, T>> {
        self.0.try_write_for(wait)
    }
}

#[cfg(test)]
mod idx_lock_tests {
    use super::IdxLock;
    use std::sync::Arc;

    #[test]
    fn a_second_read_is_granted_while_a_writer_waits() {
        let lock = Arc::new(IdxLock::new(1u32));
        let first = lock.read();
        let writer = {
            let lock = lock.clone();
            std::thread::spawn(move || *lock.write() += 1)
        };
        // give the writer time to queue behind the first read
        std::thread::sleep(std::time::Duration::from_millis(100));
        let second = lock.read();
        assert_eq!(*first + *second, 2);
        drop(second);
        drop(first);
        writer.join().unwrap();
        assert_eq!(*lock.read(), 2);
    }
}
