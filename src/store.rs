//! Index registry: one tantivy index per OpenSearch index, plus its mapping.

use anyhow::{Result, anyhow};
use parking_lot::RwLock;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::hash::BuildHasherDefault;
use std::sync::Arc;
use tantivy::schema::*;
use std::path::{Path as FsPath, PathBuf};
use tantivy::directory::MmapDirectory;
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument};

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

/// Value-kind bits recorded per field path.
pub const KIND_I64: u8 = 1;
pub const KIND_U64: u8 = 2;
pub const KIND_F64: u8 = 4;
pub const KIND_STR: u8 = 8;
pub const KIND_BOOL: u8 = 16;
/// A string that parses as a date: tantivy indexes it as a date, not as text,
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

pub fn id_fingerprint(id: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    // final mix so short ids spread across the whole 64-bit space
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h
}

pub const DYN: &str = "_dyn";
pub const RAW: &str = "_raw";

/// A stable 22-character identifier derived from the index name, in the
/// alphabet the API uses for these.
pub fn index_uuid(name: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    let mut g: u64 = h ^ 0x9e37_79b9_7f4a_7c15;
    let mut out = String::with_capacity(22);
    for i in 0..22 {
        let src = if i % 2 == 0 { &mut h } else { &mut g };
        *src = src.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        out.push(ALPHABET[((*src >> 58) & 63) as usize] as char);
    }
    out
}

/// Round to the nearest value a sixteen-bit float can hold.
pub fn half_float(v: f64) -> f64 {
    let bits = (v as f32).to_bits();
    let sign = bits >> 31;
    let exp = ((bits >> 23) & 0xff) as i32 - 127;
    // outside what the exponent can name, the value is kept as it is
    if !(-14..=15).contains(&exp) {
        return v;
    }
    let mantissa = bits & 0x007f_ffff;
    // ten bits of mantissa, rounded to nearest with ties going even
    let shift = 13;
    let round = (mantissa + (1 << (shift - 1)) + ((mantissa >> shift) & 1)) >> shift;
    let (exp, round) = if round > 0x3ff { (exp + 1, round >> 1) } else { (exp, round) };
    let out = (sign << 31) | (((exp + 127) as u32) << 23) | (round << shift);
    f32::from_bits(out) as f64
}

pub fn build_schema() -> (Schema, Fields) {
    let mut sb = Schema::builder();
    let id = sb.add_text_field("_id", STRING | STORED | FAST);
    let source = sb.add_text_field("_source", STORED);
    let dynamic = sb.add_json_field(
        DYN,
        JsonObjectOptions::default().set_fast(None).set_expand_dots_enabled().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("default")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        ),
    );
    // `_raw` keeps its own fast fields: tantivy's RangeQuery over a JSON field
    // only works on fast fields, so dropping them here breaks every range query
    // that resolves to the untokenised view. Measured: removing them buys ~5% of
    // the write path, which is not worth the semantics.
    let raw = sb.add_json_field(
        RAW,
        JsonObjectOptions::default().set_fast(Some("raw")).set_expand_dots_enabled().set_indexing_options(
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
}

impl Mapping {
    pub fn from_body(body: &Value) -> Mapping {
        let mut types = HashMap::new();
        if let Some(props) = body.get("properties").and_then(|p| p.as_object()) {
            flatten_props(props, "", &mut types);
        }
        Mapping { types, raw: body.clone() }
    }

    /// Multi-fields declared with a normalizer, as (parent path, sub name).
    ///
    /// A normalizer transforms the value at index time rather than tokenising
    /// it, so the sub-field needs its own copy of the value in the index.
    /// Add the mappings a document's new fields earn under `dynamic_templates`.
    ///
    /// Returns the offending field name when the mapping is strict about
    /// fields no template claims.
    pub fn apply_dynamic_templates(&mut self, source: &Value) -> Result<(), String> {
        let dynamic = self
            .raw
            .get("dynamic")
            .and_then(|v| v.as_str())
            .unwrap_or("true")
            .to_string();
        let templates =
            self.raw.get("dynamic_templates").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        if templates.is_empty() && !dynamic.starts_with("strict") {
            return Ok(());
        }
        let Some(obj) = source.as_object() else { return Ok(()) };
        for (name, value) in obj {
            if name.starts_with('_') || self.types.contains_key(name) {
                continue;
            }
            if self.raw.pointer(&format!("/properties/{name}")).is_some() {
                continue;
            }
            let kind = json_mapping_type(value);
            let mut matched = false;
            for t in &templates {
                let Some(spec) = t.as_object().and_then(|o| o.values().next()) else { continue };
                let pattern = spec.get("match").and_then(|v| v.as_str()).unwrap_or("*");
                if !glob_match(pattern, name) {
                    continue;
                }
                if let Some(mt) = spec.get("match_mapping_type").and_then(|v| v.as_str()) {
                    if mt != "*" && mt != kind {
                        continue;
                    }
                }
                if let Some(m) = spec.get("mapping") {
                    self.insert_property(name, m.clone());
                }
                matched = true;
                break;
            }
            if !matched && dynamic.starts_with("strict") {
                return Err(name.clone());
            }
        }
        Ok(())
    }

    fn insert_property(&mut self, name: &str, def: Value) {
        if !self.raw.is_object() {
            self.raw = serde_json::json!({});
        }
        let props = self
            .raw
            .as_object_mut()
            .unwrap()
            .entry("properties")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(o) = props.as_object_mut() {
            o.insert(name.to_string(), def.clone());
            let mut one = Map::new();
            one.insert(name.to_string(), def);
            flatten_props(&one, "", &mut self.types);
        }
    }

    /// Note the fields a document maps dynamically.
    ///
    /// Only dates are inferred. Every other type is stored the same way
    /// whether the mapping named it or not, but a date needs its own column,
    /// and nothing downstream can build one without knowing the field is one.
    pub fn learn_dynamic(&mut self, source: &Value) {
        let mut found = Vec::new();
        Self::sniff_dates(source, &mut String::new(), &self.types, &mut found);
        for path in found {
            self.types.insert(path, "date".into());
        }
    }

    fn sniff_dates(
        node: &Value,
        path: &mut String,
        known: &HashMap<String, String>,
        out: &mut Vec<String>,
    ) {
        match node {
            Value::Object(o) => {
                let base = path.len();
                for (k, v) in o {
                    if k.starts_with('_') {
                        continue;
                    }
                    if base > 0 {
                        path.push('.');
                    }
                    path.push_str(k);
                    Self::sniff_dates(v, path, known, out);
                    path.truncate(base);
                }
            }
            Value::Array(a) => {
                for v in a {
                    Self::sniff_dates(v, path, known, out);
                }
            }
            Value::String(s) => {
                // a full calendar date, not a bare year that happens to parse
                let dated = s.len() >= 10
                    && s.as_bytes()[4] == b'-'
                    && s.as_bytes()[7] == b'-'
                    && parse_date_lenient(s).is_some();
                if dated && !known.contains_key(path.as_str()) {
                    out.push(path.clone());
                }
            }
            _ => {}
        }
    }

    /// A knob declared on one field's mapping entry.
    pub fn field_option(&self, field: &str, key: &str) -> Option<Value> {
        let mut node = self.raw.get("properties")?;
        let mut segs = field.split('.').peekable();
        while let Some(seg) = segs.next() {
            node = node.as_object()?.get(seg)?;
            if segs.peek().is_some() {
                node = node.get("properties").or_else(|| node.get("fields"))?;
            }
        }
        node.get(key).cloned()
    }

    /// The normalizer a field's mapping declares, if any.
    pub fn normalizer_of(&self, field: &str) -> Option<String> {
        let (parent, sub) = field.rsplit_once('.')?;
        let props = self.raw.get("properties")?.as_object()?;
        let mut node = props.get(parent.split('.').next()?)?;
        for seg in parent.split('.').skip(1) {
            node = node.get("properties")?.as_object()?.get(seg)?;
        }
        node.get("fields")?
            .get(sub)?
            .get("normalizer")?
            .as_str()
            .map(|s| s.to_string())
    }

    pub fn normalized_subfields(&self) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        if let Some(props) = self.raw.get("properties").and_then(|p| p.as_object()) {
            collect_normalizers(props, "", &mut out);
        }
        out
    }

    /// Types the mapping treats as a single value rather than a container.
    pub fn is_leaf_type(&self, field: &str) -> bool {
        matches!(
            self.type_of(field),
            Some(t) if t.ends_with("_range") || t == "flat_object" || t == "object"
        )
    }

    pub fn type_of(&self, field: &str) -> Option<&str> {
        let field = self.target_of(field).unwrap_or(field);
        self.types.get(field).map(|s| s.as_str())
    }

    /// A field declared as an `alias` is another name for a field that is
    /// really there; this is the name behind it.
    pub fn target_of(&self, field: &str) -> Option<&str> {
        let path = self
            .raw
            .pointer(&format!("/properties/{}", field.replace('.', "/properties/")))?;
        if path.get("type").and_then(|t| t.as_str()) != Some("alias") {
            return None;
        }
        path.get("path").and_then(|p| p.as_str())
    }

    /// PUT _mapping is additive: new properties layer onto the old ones, and
    /// top-level knobs like `dynamic` are replaced.
    pub fn merge(&mut self, body: &Value) {
        if !self.raw.is_object() {
            self.raw = serde_json::json!({});
        }
        let Some(incoming) = body.as_object() else { return };
        for (key, val) in incoming {
            if key == "properties" {
                if let Some(props) = val.as_object() {
                    flatten_props(props, "", &mut self.types);
                    let slot = self
                        .raw
                        .as_object_mut()
                        .unwrap()
                        .entry("properties")
                        .or_insert_with(|| serde_json::json!({}));
                    if let Some(existing) = slot.as_object_mut() {
                        for (k, v) in props {
                            // a dotted name is an object with one field in it,
                            // written short; the mapping holds the long form
                            match k.split_once('.') {
                                Some((head, rest)) => {
                                    let parent = existing
                                        .entry(head.to_string())
                                        .or_insert_with(|| serde_json::json!({}));
                                    let inner = parent
                                        .as_object_mut()
                                        .map(|o| {
                                            o.entry("properties".to_string())
                                                .or_insert_with(|| serde_json::json!({}))
                                        });
                                    if let Some(inner) = inner.and_then(|i| i.as_object_mut()) {
                                        inner.insert(rest.to_string(), v.clone());
                                    }
                                }
                                None => {
                                    existing.insert(k.clone(), v.clone());
                                }
                            }
                        }
                    }
                }
            } else {
                self.raw.as_object_mut().unwrap().insert(key.clone(), val.clone());
            }
        }
    }
}

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
                    if crate::query::parse_datetime(s).is_some() { KIND_DATE } else { KIND_STR }
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

fn collect_normalizers(
    props: &Map<String, Value>,
    prefix: &str,
    out: &mut Vec<(String, String, String)>,
) {
    for (name, def) in props {
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        if let Some(subs) = def.get("fields").and_then(|f| f.as_object()) {
            for (sub, sdef) in subs {
                // a multi-field without a normalizer still needs its own copy
                // of the value; nothing else populates that path
                let n = sdef.get("normalizer").and_then(|v| v.as_str()).unwrap_or("");
                out.push((path.clone(), sub.clone(), n.to_string()));
            }
        }
        if let Some(inner) = def.get("properties").and_then(|p| p.as_object()) {
            collect_normalizers(inner, &path, out);
        }
    }
}

fn flatten_props(props: &Map<String, Value>, prefix: &str, out: &mut HashMap<String, String>) {
    for (name, def) in props {
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        if let Some(sub) = def.get("properties").and_then(|p| p.as_object()) {
            flatten_props(sub, &path, out);
            continue;
        }
        if let Some(t) = def.get("type").and_then(|t| t.as_str()) {
            out.insert(path.clone(), t.to_string());
        }
        // multi-fields: `title.keyword`
        if let Some(subs) = def.get("fields").and_then(|f| f.as_object()) {
            flatten_props(subs, &path, out);
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DocMeta {
    pub version: u64,
    pub live: bool,
}

pub struct IdxState {
    pub name: String,
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
    /// per-segment block statistics, built on demand
    pub stats: Arc<crate::blockstats::StatsCache>,
    /// False while the id table is still being rebuilt after a reopen. Until it
    /// flips, an unknown id has to be checked against the index itself.
    pub ids_loaded: Arc<std::sync::atomic::AtomicBool>,
}

impl IdxState {
    /// Persist the learned field information next to the index so a reopen does
    /// not lose dynamic mappings or the range-narrowing kinds.
    pub fn save_meta(&self) {
        let Some(path) = &self.path else { return };
        let meta = serde_json::json!({
            "name": self.name,
            "body": {"mappings": self.mapping.raw, "settings": self.settings},
            "dynamic_types": self.dynamic_types,
            "observed_kinds": self.observed_kinds,
        });
        let _ = std::fs::write(path.join("_meta.json"), meta.to_string());
    }

    /// Make everything written so far visible to search.
    pub fn refresh(&mut self) -> Result<()> {
        // nothing was ever written, so there is nothing to commit
        if let Some(w) = self.writer.as_mut() {
            w.commit()?;
        }
        self.save_meta();
        self.reader.reload()?;
        self.realtime.reload()?;
        self.pending.clear();
        self.pending_seq.clear();
        self.pending_bytes = 0;
        Ok(())
    }

    /// Bound how much un-refreshed source we hold in memory. Flushing advances
    /// only the realtime reader, so search visibility is unchanged.
    pub fn note_pending_seq(&mut self, id: &str, seq: u64) {
        self.pending_seq.insert(id.to_string(), seq);
    }

    pub fn note_pending(&mut self, id: &str, source: Option<String>) {
        self.pending_bytes += id.len() + source.as_ref().map(|s| s.len()).unwrap_or(0) + 48;
        self.pending.insert(id.to_string(), source);
        if self.pending_bytes > PENDING_BUDGET_BYTES {
            let committed = self.writer.as_mut().map(|w| w.commit().is_ok()).unwrap_or(false);
            if committed {
                let _ = self.realtime.reload();
                self.pending.clear();
                self.pending_bytes = 0;
            }
        }
    }

    /// Bytes each fast-field column occupies. This is the closest honest
    /// analogue of what OpenSearch reports as fielddata.
    pub fn field_column_bytes(&self) -> HashMap<String, u64> {
        let mut out: HashMap<String, u64> = HashMap::new();
        let searcher = self.reader.searcher();
        for seg in searcher.segment_readers() {
            let ff = seg.fast_fields();
            for (path, _) in self.all_field_types() {
                for prefix in [DYN, RAW] {
                    let col = format!("{prefix}.{path}");
                    if let Ok(bytes) = ff.column_num_bytes(&col) {
                        let n = bytes.get_bytes();
                        if n > 0 {
                            *out.entry(path.clone()).or_insert(0) += n;
                        }
                    }
                }
            }
        }
        out
    }

    pub fn has_writer(&self) -> bool {
        self.writer.is_some()
    }

    /// The writer, created on demand.
    pub fn writer(&mut self) -> Result<&mut IndexWriter> {
        self.last_write = std::time::Instant::now();
        if self.writer.is_none() {
            self.writer = Some(
                self.index
                    .writer_with_num_threads(self.writer_threads.max(1), self.writer_budget)?,
            );
        }
        Ok(self.writer.as_mut().unwrap())
    }

    /// Give back the indexing threads and arena for an index that has gone
    /// quiet. The writer is only a cache: committing first makes everything it
    /// held durable, so nothing is lost by dropping it.
    ///
    /// Buffered writes are not a reason to refuse. They were, which meant a bulk
    /// load could never release anything -- the buffer is never empty mid-load,
    /// which is exactly when the writers pile up.
    pub fn release_idle_writer(&mut self, idle_for: std::time::Duration) -> bool {
        if self.writer.is_none() || self.last_write.elapsed() < idle_for {
            return false;
        }
        if let Some(mut w) = self.writer.take() {
            if w.commit().is_err() {
                // could not flush cleanly: keep it rather than lose the writes
                self.writer = Some(w);
                return false;
            }
            let _ = w.wait_merging_threads();
        }
        // The realtime reader has to advance so GET still answers from the index
        // now that the buffer is gone. The search reader deliberately does not:
        // a write must stay invisible to search until an explicit refresh.
        let _ = self.realtime.reload();
        self.pending.clear();
        self.pending_seq.clear();
        self.pending_bytes = 0;
        release_freed_memory();
        true
    }

    /// Next version for a document id, and the sequence number of the write.
    ///
    /// `existed` must be the answer the caller already got from `is_live`, so a
    /// write cannot decide "updated" and "version 1" from two different sources
    /// while the id table is still loading.
    /// Record a version the caller chose rather than the next one in sequence.
    ///
    /// External versioning hands the index a number kept somewhere else, so
    /// the index follows it rather than counting for itself.
    pub fn bump_to(&mut self, id: &str, live: bool, version: u64) -> (u64, u64) {
        let fp = id_fingerprint(id);
        self.versions.insert(id.to_string(), DocMeta { version, live });
        if live {
            self.live_ids.insert(fp);
        }
        let seq = self.seq_no;
        self.seq_no += 1;
        (version, seq)
    }

    pub fn bump(&mut self, id: &str, live: bool, existed: bool) -> (u64, u64) {
        let fp = id_fingerprint(id);
        let known = existed || self.versions.contains_key(id);
        let version = if known {
            let m = self
                .versions
                .entry(id.to_string())
                .or_insert(DocMeta { version: 1, live: true });
            m.version += 1;
            m.live = live;
            m.version
        } else {
            // brand new: version 1 needs no exact entry, only the fingerprint
            1
        };
        if live {
            self.live_ids.insert(fp);
        } else {
            // a tombstone is recorded exactly; removing the fingerprint could
            // take a colliding id's liveness with it
            self.versions.insert(id.to_string(), DocMeta { version, live: false });
        }
        let seq = self.seq_no;
        self.seq_no += 1;
        (version, seq)
    }

    /// A stable identifier for the index's current commit point.
    pub fn commit_id(&self) -> String {
        self.index
            .searchable_segment_ids()
            .ok()
            .and_then(|ids| ids.first().map(|i| i.uuid_string()))
            .unwrap_or_else(|| "0".repeat(22))
    }

    pub fn version_of(&self, id: &str) -> u64 {
        self.versions.get(id).map(|m| m.version).unwrap_or(1)
    }

    /// Is there a live document under this id?
    pub fn is_live(&self, id: &str) -> bool {
        match self.pending.get(id) {
            Some(Some(_)) => true,
            Some(None) => false,
            None => match self.versions.get(id) {
                Some(m) => m.live,
                None => {
                    if !self.ids_loaded.load(std::sync::atomic::Ordering::Relaxed) {
                        // table still filling in after a reopen
                        return self.lookup_id(id);
                    }
                    // a fingerprint miss is authoritative; a hit is confirmed
                    // against the index, since fingerprints can collide
                    self.live_ids.contains(&id_fingerprint(id)) && self.lookup_id(id)
                }
            },
        }
    }

    fn lookup_id(&self, id: &str) -> bool {
        let searcher = self.realtime.searcher();
        let q = tantivy::query::TermQuery::new(
            Term::from_field_text(self.fields.id, id),
            tantivy::schema::IndexRecordOption::Basic,
        );
        searcher.search(&q, &tantivy::collector::Count).map(|c| c > 0).unwrap_or(false)
    }

    /// Scan the committed index for live document ids. Runs off the write lock
    /// so a reopen does not stall startup.
    pub fn scan_ids(reader: &IndexReader, id_field: Field) -> Vec<u64> {
        let mut out = Vec::new();
        let searcher = reader.searcher();
        for seg in searcher.segment_readers() {
            let Ok(Some(col)) = seg.fast_fields().str("_id") else { continue };
            let alive = seg.alive_bitset();
            let mut buf = Vec::new();
            for doc in 0..seg.max_doc() {
                if alive.map(|a| !a.is_alive(doc)).unwrap_or(false) {
                    continue;
                }
                let Some(ord) = col.term_ords(doc).next() else { continue };
                buf.clear();
                if col.ord_to_bytes(ord, &mut buf).unwrap_or(false) {
                    if let Ok(id) = std::str::from_utf8(&buf) {
                        out.push(id_fingerprint(id));
                    }
                }
            }
        }
        let _ = id_field;
        out
    }

    /// Merge a scan result in without overwriting anything written since.
    pub fn absorb_ids(&mut self, scanned: Vec<u64>) {
        for fp in scanned {
            self.live_ids.insert(fp);
        }
        self.ids_loaded.store(true, std::sync::atomic::Ordering::Release);
    }

    /// Settings echoed back by GET _settings, including the defaults the
    /// YAML suite asserts on.
    pub fn effective_settings(&self) -> Value {
        let mut idx = serde_json::json!({
            "number_of_shards": "1",
            "number_of_replicas": "1",
            "provided_name": self.name,
        });
        // settings arrive either nested under `index` or flat, and OpenSearch
        // always echoes the values back as strings
        fn put(idx: &mut Value, k: &str, v: &Value) {
            let k = k.trim_start_matches("index.");
            idx[k] = match v {
                Value::String(_) => v.clone(),
                Value::Null => return,
                other => Value::String(other.to_string()),
            };
        }
        if let Some(user) = self.settings.as_object() {
            for (k, v) in user {
                if k == "index" {
                    if let Some(nested) = v.as_object() {
                        for (k2, v2) in nested {
                            put(&mut idx, k2, v2);
                        }
                    }
                } else {
                    put(&mut idx, k, v);
                }
            }
        }
        serde_json::json!({ "index": idx })
    }

    /// Record the dynamic types a document contributes.
    ///
    /// Bulk loads send the same shape over and over, so remember which shapes
    /// have been walked and skip the walk for repeats.
    pub fn observe(&mut self, source: &Value) {
        // kinds are always tracked: two documents can share a shape and still
        // differ in value type, and a missed kind means missed hits
        let mut path = std::mem::take(&mut self.kind_path_buf);
        path.clear();
        observe_kinds(source, &mut path, &mut self.observed_kinds);
        self.kind_path_buf = path;
        if let Some(obj) = source.as_object() {
            let mut sig: u64 = 0xcbf2_9ce4_8422_2325;
            for k in obj.keys() {
                for b in k.as_bytes() {
                    sig ^= *b as u64;
                    sig = sig.wrapping_mul(0x1000_0000_01b3);
                }
                sig ^= 0xff;
            }
            if !self.seen_shapes.insert(sig) {
                return;
            }
        }
        self.observe_inner(source)
    }

    fn observe_inner(&mut self, source: &Value) {
        fn walk(v: &Value, prefix: &str, out: &mut HashMap<String, String>) {
            match v {
                Value::Object(o) => {
                    for (k, child) in o {
                        let path =
                            if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                        walk(child, &path, out);
                    }
                }
                Value::Array(a) => {
                    for x in a {
                        walk(x, prefix, out);
                    }
                }
                leaf if !prefix.is_empty() => {
                    let kind = match leaf {
                        Value::String(s) => {
                            if crate::query::parse_datetime(s).is_some() { "date" } else { "text" }
                        }
                        Value::Bool(_) => "boolean",
                        Value::Number(n) if n.is_f64() && n.as_i64().is_none() => "float",
                        Value::Number(_) => "long",
                        _ => return,
                    };
                    out.entry(prefix.to_string()).or_insert_with(|| kind.to_string());
                }
                _ => {}
            }
        }
        walk(source, "", &mut self.dynamic_types);
    }

    /// Every field path known for this index, explicit mappings taking priority.
    pub fn all_field_types(&self) -> Vec<(String, String)> {
        let mut out: HashMap<String, String> = self.dynamic_types.clone();
        for (k, v) in &self.mapping.types {
            out.insert(k.clone(), v.clone());
        }
        let mut v: Vec<(String, String)> = out.into_iter().collect();
        v.sort();
        v
    }

    /// Look a setting up without building the merged view.
    ///
    /// `effective_settings` allocates a fresh JSON object to fold the defaults
    /// in, which is the right shape for GET _settings but far too much work for
    /// a value read on every shard of every query.
    fn raw_setting(&self, key: &str) -> Option<&Value> {
        let settings = self.settings.as_object()?;
        if let Some(nested) = settings.get("index").and_then(|v| v.as_object()) {
            // a setting written flat keeps the `index.` prefix it arrived with,
            // even once it is filed under `index`
            if let Some(v) = nested
                .get(key)
                .or_else(|| nested.get(&format!("index.{key}")))
                .filter(|v| !v.is_null())
            {
                return Some(v);
            }
        }
        settings
            .get(key)
            .or_else(|| settings.get(&format!("index.{key}")))
            .filter(|v| !v.is_null())
    }

    /// Every live document's id, in the order the segments hold them.
    pub fn all_ids(&self) -> Vec<String> {
        let searcher = self.realtime.searcher();
        let mut out = Vec::new();
        for reader in searcher.segment_readers() {
            let Ok(col) = reader.fast_fields().str("_id") else { continue };
            let Some(col) = col else { continue };
            let alive = reader.alive_bitset();
            for doc in 0..reader.max_doc() {
                if alive.map(|a| a.is_deleted(doc)).unwrap_or(false) {
                    continue;
                }
                for ord in col.term_ords(doc) {
                    let mut buf = String::new();
                    if col.ord_to_str(ord, &mut buf).is_ok() {
                        out.push(buf);
                    }
                    break;
                }
            }
        }
        // a write not yet visible to the reader is still part of the index
        for (id, held) in &self.pending {
            if held.is_some() && !out.contains(id) {
                out.push(id.clone());
            }
        }
        out
    }

    /// The moment now, written the way a timestamp is reported.
    pub fn now_iso() -> String {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i128)
            .unwrap_or(0);
        tantivy::time::OffsetDateTime::from_unix_timestamp_nanos(ms * 1_000_000)
            .map(|d| {
                format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
                    d.year(),
                    d.month() as u8,
                    d.day(),
                    d.hour(),
                    d.minute(),
                    d.second(),
                    d.millisecond(),
                )
            })
            .unwrap_or_default()
    }

    pub fn created_millis(&self) -> u64 {
        // a setting written by hand wins, since a restored index keeps the
        // date it was first made
        self.numeric_setting("creation_date").unwrap_or(self.created_ms)
    }

    /// The creation date as text, which is the other spelling `_cat` offers.
    pub fn created_string(&self) -> String {
        let ms = self.created_millis() as i128;
        tantivy::time::OffsetDateTime::from_unix_timestamp_nanos(ms * 1_000_000)
            .map(|d| format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
                d.year(), d.month() as u8, d.day(), d.hour(), d.minute(), d.second(),
                d.millisecond(),
            ))
            .unwrap_or_default()
    }

    pub fn numeric_setting(&self, key: &str) -> Option<u64> {
        match self.raw_setting(key)? {
            Value::String(s) => s.parse().ok(),
            Value::Number(n) => n.as_u64(),
            _ => None,
        }
    }

    /// How many shards the index reports. Read per shard of every query.
    pub fn shard_count(&self) -> u64 {
        self.numeric_setting("number_of_shards").unwrap_or(1)
    }

    /// Look a setting up by dotted name, accepting the flat or nested form.
    pub fn setting(&self, key: &str) -> Option<String> {
        let settings = self.effective_settings();
        let flat = settings.pointer(&format!("/index/{key}"));
        let nested = settings.pointer(&format!("/index/{}", key.replace('.', "/")));
        let prefixed = settings.pointer(&format!("/index/index.{key}"));
        flat.or(nested).or(prefixed).map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
    }

    /// `index.max_terms_count` caps how many terms a `terms` query may carry.
    /// `index.max_regex_length` caps how long a pattern a query may carry.
    pub fn max_regex_length(&self) -> usize {
        self.numeric_setting("max_regex_length").unwrap_or(1_000) as usize
    }

    pub fn max_terms_count(&self) -> usize {
        self.numeric_setting("max_terms_count").unwrap_or(65_536) as usize
    }

    pub fn next_auto_id(&mut self) -> String {
        self.auto_id += 1;
        format!("auto-{:016x}", self.auto_id)
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
    scroll_seq: Arc<std::sync::atomic::AtomicU64>,
    /// One search thread pool for the whole process. Giving each index its own
    /// costs a pool per index, which is invisible with one index and ruinous
    /// with hundreds.
    executor: tantivy::Executor,
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
        let id = format!("obsearch-pit-{n:016x}");
        let mut ceiling = HashMap::new();
        for name in self.resolve(expr) {
            if let Some(st) = self.get(&name) {
                ceiling.insert(name, st.read().seq_no);
            }
        }
        self.pits.write().insert(
            id.clone(),
            PitState { expr: expr.to_string(), ceiling, keep_alive_ms },
        );
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

fn shared_executor() -> tantivy::Executor {
    let threads = std::env::var("OBSEARCH_SEARCH_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4));
    if threads <= 1 {
        return tantivy::Executor::single_thread();
    }
    tantivy::Executor::multi_thread(threads, "obsearch-search-")
        .unwrap_or_else(|_| tantivy::Executor::single_thread())
}

/// Put an alias definition into the form it is read back in.
///
/// `routing` is shorthand: it sets the routing used for indexing and the one
/// used for searching at once, and only those two are ever reported. Anything
/// else the caller wrote -- a filter, is_write_index -- is kept as it stands.
pub fn normalize_alias(def: &Value) -> Value {
    let Some(obj) = def.as_object() else { return serde_json::json!({}) };
    let mut out = obj.clone();
    if let Some(r) = out.remove("routing") {
        for key in ["index_routing", "search_routing"] {
            out.entry(key.to_string()).or_insert_with(|| r.clone());
        }
    }
    // a routing value is a string even when it was written as a number
    for key in ["index_routing", "search_routing"] {
        if let Some(v) = out.get(key) {
            if let Some(n) = v.as_i64() {
                out.insert(key.to_string(), Value::String(n.to_string()));
            } else if let Some(f) = v.as_f64() {
                out.insert(key.to_string(), Value::String(f.to_string()));
            }
        }
    }
    Value::Object(out)
}

/// Index names are not path-safe, so each one gets a stable encoded directory.
fn dir_name(index: &str) -> String {
    let mut out = String::new();
    for b in index.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02x}")),
        }
    }
    out
}

impl Store {
    /// Periodically hand back indexing resources for indices that have gone
    /// quiet. With one index this is invisible; with hundreds it is the
    /// difference between 13 MB per index and nothing.
    fn start_writer_reaper(&self) {
        let idle_secs: u64 = std::env::var("OBSEARCH_WRITER_IDLE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);
        if idle_secs == 0 {
            return;
        }
        let store = self.clone();
        std::thread::spawn(move || {
            let idle = std::time::Duration::from_secs(idle_secs);
            loop {
                std::thread::sleep(std::time::Duration::from_secs(idle_secs.max(1) / 2 + 1));
                for name in store.names() {
                    if let Some(st) = store.get(&name) {
                        if st.write().release_idle_writer(idle) {
                            store.note_writer_closed(&name);
                        }
                    }
                }
            }
        });
    }

    pub fn new() -> Store {
        let store = Store {
            inner: Arc::new(RwLock::new(HashMap::new())),
            data_dir: None,
            executor: shared_executor(),
            live_writers: Arc::new(RwLock::new(Vec::new())),
            cluster_settings: Arc::new(RwLock::new(serde_json::json!({"persistent": {}, "transient": {}}))),
            voting_exclusions: Arc::new(RwLock::new(Vec::new())),
            components: Arc::new(RwLock::new(HashMap::new())),
            pits: Arc::new(RwLock::new(HashMap::new())),
            data_streams: Arc::new(RwLock::new(HashMap::new())),
            pit_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            templates: Arc::new(RwLock::new(HashMap::new())),
            scrolls: Arc::new(RwLock::new(HashMap::new())),
            scroll_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        store.start_writer_reaper();
        store
    }

    /// Back indices with mmapped files under `dir`, and reopen whatever is
    /// already there. Keeps the index out of process memory: the OS page cache
    /// holds it instead, and it survives a restart.
    pub fn on_disk(dir: impl AsRef<FsPath>) -> Result<Store> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let store = Store {
            inner: Arc::new(RwLock::new(HashMap::new())),
            data_dir: Some(dir.clone()),
            executor: shared_executor(),
            live_writers: Arc::new(RwLock::new(Vec::new())),
            cluster_settings: Arc::new(RwLock::new(serde_json::json!({"persistent": {}, "transient": {}}))),
            voting_exclusions: Arc::new(RwLock::new(Vec::new())),
            components: Arc::new(RwLock::new(HashMap::new())),
            pits: Arc::new(RwLock::new(HashMap::new())),
            data_streams: Arc::new(RwLock::new(HashMap::new())),
            pit_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            templates: Arc::new(RwLock::new(HashMap::new())),
            scrolls: Arc::new(RwLock::new(HashMap::new())),
            scroll_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let meta_path = entry.path().join("_meta.json");
            let Ok(raw) = std::fs::read_to_string(&meta_path) else { continue };
            let Ok(meta): std::result::Result<Value, _> = serde_json::from_str(&raw) else {
                continue;
            };
            let Some(name) = meta.get("name").and_then(|v| v.as_str()) else { continue };
            let body = meta.get("body").cloned().unwrap_or_else(|| serde_json::json!({}));
            let learned = (
                meta.get("dynamic_types").cloned(),
                meta.get("observed_kinds").cloned(),
            );
            match store.open_index(name, &body, entry.path()) {
                Ok(()) => {
                    // Rebuild the id table in the background: startup no longer
                    // waits on a full scan of every document.
                    if let Some(st) = store.get(name) {
                        {
                            let mut g = st.write();
                            if let Some(v) = learned.0.and_then(|v| serde_json::from_value(v).ok()) {
                                g.dynamic_types = v;
                            }
                            match learned.1.and_then(|v| serde_json::from_value(v).ok()) {
                                Some(v) => g.observed_kinds = v,
                                // no kinds recorded: treat what we learn from
                                // here on as partial and never narrow with it
                                None => g.kinds_complete = false,
                            }
                        }
                        let (reader, id_field, flag) = {
                            let g = st.read();
                            (g.realtime.clone(), g.fields.id, g.ids_loaded.clone())
                        };
                        flag.store(false, std::sync::atomic::Ordering::Release);
                        let st2 = st.clone();
                        std::thread::spawn(move || {
                            let scanned = IdxState::scan_ids(&reader, id_field);
                            st2.write().absorb_ids(scanned);
                        });
                    }
                }
                Err(e) => tracing::warn!("could not reopen index {name}: {e}"),
            }
        }
        store.start_writer_reaper();
        Ok(store)
    }

    /// How many indices may hold a writer at once.
    pub fn writer_limit() -> usize {
        std::env::var("OBSEARCH_MAX_LIVE_WRITERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8)
            .max(1)
    }

    /// Record that `name` now holds a writer, releasing the least recently
    /// written index's writer if that puts us over the limit.
    pub fn note_writer_opened(&self, name: &str) {
        let evict = {
            let mut live = self.live_writers.write();
            live.retain(|n| n != name);
            live.push(name.to_string());
            let limit = Self::writer_limit();
            if live.len() > limit { Some(live.remove(0)) } else { None }
        };
        if let Some(victim) = evict {
            if let Some(st) = self.get(&victim) {
                st.write().release_idle_writer(std::time::Duration::ZERO);
            }
        }
    }

    pub fn note_writer_closed(&self, name: &str) {
        self.live_writers.write().retain(|n| n != name);
    }

    fn index_path(&self, name: &str) -> Option<PathBuf> {
        self.data_dir.as_ref().map(|d| d.join(dir_name(name)))
    }

    pub fn exists(&self, name: &str) -> bool {
        self.inner.read().contains_key(name)
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.inner.read().keys().cloned().collect();
        v.sort();
        v
    }

    /// Is this name an alias rather than an index of its own?
    pub fn is_alias(&self, name: &str) -> bool {
        self.inner.read().values().any(|st| st.read().aliases.contains_key(name))
    }

    pub fn get(&self, name: &str) -> Option<Arc<RwLock<IdxState>>> {
        if let Some(s) = self.inner.read().get(name) {
            return Some(s.clone());
        }
        // alias lookup
        let guard = self.inner.read();
        for st in guard.values() {
            if st.read().aliases.contains_key(name) {
                return Some(st.clone());
            }
        }
        None
    }

    /// Resolve an index expression (`test`, `test*`, `_all`, `a,b`) to concrete indices.
    /// Which indices a name or pattern addresses.
    ///
    /// Closed indices are included: most APIs -- delete, stats, the cat
    /// endpoints -- are meant to see them. Searching is the exception, and
    /// asks for `resolve_open` instead.
    pub fn resolve(&self, expr: &str) -> Vec<String> {
        self.resolve_with(expr, true)
    }

    /// As `resolve`, but a pattern passes over the closed indices, which is
    /// what `expand_wildcards` defaults to when reading documents. A name
    /// given outright still resolves, so the caller can say it is closed
    /// rather than that it does not exist.
    pub fn resolve_open(&self, expr: &str) -> Vec<String> {
        self.resolve_with(expr, false)
    }

    pub fn is_closed(&self, name: &str) -> bool {
        self.get(name).map(|st| st.read().closed).unwrap_or(false)
    }

    /// Every index carrying this alias, in name order.
    pub fn indices_for_alias(&self, alias: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .inner
            .read()
            .iter()
            .filter(|(_, st)| st.read().aliases.contains_key(alias))
            .map(|(name, _)| name.clone())
            .collect();
        out.sort();
        out
    }

    fn resolve_with(&self, expr: &str, include_closed: bool) -> Vec<String> {
        let open_only = |names: Vec<String>| -> Vec<String> {
            if include_closed {
                return names;
            }
            names.into_iter().filter(|n| !self.is_closed(n)).collect()
        };
        if expr.is_empty() || expr == "_all" || expr == "*" {
            return open_only(self.names());
        }
        let mut out = Vec::new();
        for part in expr.split(',') {
            let part = part.trim();
            // a name in angle brackets is a date expression standing for the
            // index of some day, month or year
            let resolved;
            let part = if part.starts_with('<') {
                resolved = resolve_date_math_name(part);
                resolved.as_str()
            } else {
                part
            };
            if part.contains('*') {
                let re = wildcard_to_regex(part);
                // a pattern reaches an index by its own name or by any alias
                // standing in front of it
                for n in open_only(self.names()) {
                    let by_alias = self
                        .get(&n)
                        .map(|st| st.read().aliases.keys().any(|a| re.is_match(a)))
                        .unwrap_or(false);
                    if (re.is_match(&n) || by_alias) && !out.contains(&n) {
                        out.push(n);
                    }
                }
            } else if self.exists(part) {
                if !out.contains(&part.to_string()) {
                    out.push(part.to_string());
                }
            } else {
                // an alias may stand in front of several indices, and names
                // all of them; `get` would answer with whichever it found
                // first, which is how a search over an alias came to miss
                // every index but one
                let mut named = self.indices_for_alias(part);
                if named.is_empty() {
                    if let Some(st) = self.get(part) {
                        named.push(st.read().name.clone());
                    }
                }
                for n in open_only(named) {
                    if !out.contains(&n) {
                        out.push(n);
                    }
                }
            }
        }
        out
    }

    /// `size` is how many documents each batch returns; the cursor is placed
    /// after the batch the opening search already delivered.
    pub fn open_scroll(&self, expr: &str, body: &Value, size: usize) -> String {
        let n = self.scroll_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!("obsearch-scroll-{n:016x}");
        self.scrolls.write().insert(
            id.clone(),
            ScrollState {
                expr: expr.to_string(),
                body: body.clone(),
                offset: size,
                size,
                pit: self.open_pit(expr, 0),
            },
        );
        id
    }

    pub fn read_scroll(&self, id: &str) -> Option<ScrollState> {
        self.scrolls.read().get(id).cloned()
    }

    pub fn advance_scroll(&self, id: &str, by: usize) {
        if let Some(s) = self.scrolls.write().get_mut(id) {
            s.offset += by;
        }
    }

    pub fn close_scroll(&self, id: &str) -> bool {
        self.scrolls.write().remove(id).is_some()
    }

    pub fn close_all_scrolls(&self) -> usize {
        let mut s = self.scrolls.write();
        let n = s.len();
        s.clear();
        n
    }

    /// Index templates, applied to any index created with a matching name.
    pub fn put_template(&self, name: &str, body: Value) {
        self.templates.write().insert(name.to_string(), body);
    }

    /// The data streams there are, each with the template it was made from.
    pub fn data_streams(&self) -> HashMap<String, String> {
        self.data_streams.read().clone()
    }

    pub fn add_data_stream(&self, name: &str, template: &str) {
        self.data_streams.write().insert(name.to_string(), template.to_string());
    }

    pub fn remove_data_stream(&self, name: &str) -> Vec<String> {
        let mut streams = self.data_streams.write();
        let gone: Vec<String> = streams
            .keys()
            .filter(|k| k.as_str() == name || wildcard_to_regex(name).is_match(k))
            .cloned()
            .collect();
        for g in &gone {
            streams.remove(g);
        }
        gone
    }

    pub fn get_templates(&self) -> HashMap<String, Value> {
        self.templates.read().clone()
    }

    pub fn delete_template(&self, name: &str) -> bool {
        let mut t = self.templates.write();
        let pats: Vec<String> = t
            .keys()
            .filter(|k| k.as_str() == name || wildcard_to_regex(name).is_match(k))
            .cloned()
            .collect();
        let hit = !pats.is_empty();
        for p in pats {
            t.remove(&p);
        }
        hit
    }

    /// Merge every template whose pattern matches, lowest order first, so an
    /// index picks up the mappings and settings it was meant to be born with.
    fn apply_templates(&self, index: &str, body: &Value) -> Value {
        let templates = self.templates.read();
        let mut matched: Vec<(i64, &Value)> = templates
            .values()
            .filter(|t| {
                let pats = t
                    .get("index_patterns")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                pats.iter()
                    .filter_map(|p| p.as_str())
                    .any(|p| p == index || wildcard_to_regex(p).is_match(index))
            })
            .map(|t| (t.get("order").and_then(|o| o.as_i64()).unwrap_or(0), t))
            .collect();
        matched.sort_by_key(|(o, _)| *o);
        if matched.is_empty() {
            return body.clone();
        }
        let mut merged = serde_json::json!({});
        for (_, t) in matched {
            for key in ["settings", "mappings", "aliases"] {
                if let Some(v) = t.get(key) {
                    let slot = merged.as_object_mut().unwrap().entry(key).or_insert(serde_json::json!({}));
                    deep_merge(slot, v);
                }
            }
        }
        // the request itself always wins over a template
        deep_merge(&mut merged, body);
        merged
    }

    pub fn create(&self, name: &str, body: &Value) -> Result<()> {
        let body = &self.apply_templates(name, body);
        if self.exists(name) {
            return Err(anyhow!("resource_already_exists_exception"));
        }
        match self.index_path(name) {
            Some(path) => {
                std::fs::create_dir_all(&path)?;
                std::fs::write(
                    path.join("_meta.json"),
                    serde_json::json!({"name": name, "body": body}).to_string(),
                )?;
                self.open_index(name, body, path.clone())?;
                if let Some(st) = self.get(name) {
                    st.write().path = Some(path);
                }
                Ok(())
            }
            None => self.open_index_in_ram(name, body),
        }
    }

    fn open_index(&self, name: &str, body: &Value, path: PathBuf) -> Result<()> {
        let (schema, fields) = build_schema();
        let dir = MmapDirectory::open(&path)?;
        let index = Index::open_or_create(dir, schema)?;
        self.finish_open(name, body, index, fields)?;
        if let Some(st) = self.get(name) {
            st.write().path = Some(path);
        }
        Ok(())
    }

    fn open_index_in_ram(&self, name: &str, body: &Value) -> Result<()> {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        self.finish_open(name, body, index, fields)
    }

    fn finish_open(
        &self,
        name: &str,
        body: &Value,
        mut index: Index,
        fields: Fields,
    ) -> Result<()> {
        index.set_executor(self.executor.clone());
        // one arena per indexing thread; a bigger budget means fewer segment
        // flushes and less merging, at the cost of resident memory
        let writer_budget: usize = std::env::var("OBSEARCH_WRITER_BUDGET_MB")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(64)
            * 1024
            * 1024;
        let writer_threads: usize = std::env::var("OBSEARCH_WRITER_THREADS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2);
        let reader = index.reader_builder().reload_policy(tantivy::ReloadPolicy::Manual).try_into()?;
        let realtime =
            index.reader_builder().reload_policy(tantivy::ReloadPolicy::Manual).try_into()?;
        let mapping = body
            .get("mappings")
            .map(Mapping::from_body)
            .unwrap_or_else(|| Mapping { types: HashMap::new(), raw: serde_json::json!({}) });
        let settings = body.get("settings").cloned().unwrap_or_else(|| serde_json::json!({}));
        let aliases: HashMap<String, Value> = body
            .get("aliases")
            .and_then(|a| a.as_object())
            .map(|o| o.iter().map(|(k, v)| (k.clone(), normalize_alias(v))).collect())
            .unwrap_or_default();
        let st = IdxState {
            name: name.to_string(),
            index,
            writer: None,
            writer_threads,
            writer_budget,
            last_write: std::time::Instant::now(),
            reader,
            fields,
            mapping,
            settings,
            aliases,
            closed: false,
            versions: HashMap::new(),
            routing: HashMap::new(),
            uuid: index_uuid(&name),
            created_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            live_ids: Default::default(),
            pending: HashMap::new(),
            pending_seq: HashMap::new(),
            pending_bytes: 0,
            realtime,
            seq_no: 0,
            search_count: std::sync::atomic::AtomicU64::new(0),
            request_cache_miss: std::sync::atomic::AtomicU64::new(0),
            search_groups: RwLock::new(HashMap::new()),
            loaded_fielddata: RwLock::new(std::collections::HashSet::new()),
            auto_id: 0,
            dynamic_types: HashMap::new(),
            seen_shapes: std::collections::HashSet::new(),
            observed_kinds: HashMap::new(),
            kinds_complete: true,
            has_doc_count: false,
            noop_updates: std::sync::atomic::AtomicU64::new(0),
            flushes: std::sync::atomic::AtomicU64::new(0),
            gets: std::sync::atomic::AtomicU64::new(0),
            bytes: std::sync::atomic::AtomicU64::new(0),
            kind_path_buf: String::new(),
            path: None,
            stats: Arc::new(crate::blockstats::StatsCache::default()),
            ids_loaded: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        self.inner.write().insert(name.to_string(), Arc::new(RwLock::new(st)));
        Ok(())
    }

    /// Auto-create on first write, the way OpenSearch does.
    pub fn ensure(&self, name: &str) -> Result<Arc<RwLock<IdxState>>> {
        if let Some(s) = self.get(name) {
            return Ok(s);
        }
        self.create(name, &serde_json::json!({}))?;
        self.get(name).ok_or_else(|| anyhow!("index vanished"))
    }

    pub fn delete(&self, name: &str) -> bool {
        let targets = self.resolve(name);
        let mut guard = self.inner.write();
        let mut any = false;
        for t in targets {
            any |= guard.remove(&t).is_some();
            if let Some(path) = self.index_path(&t) {
                let _ = std::fs::remove_dir_all(path);
            }
        }
        any
    }
}

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

/// Recursive object merge; `patch` wins on conflict.
pub fn deep_merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                match b.get_mut(k) {
                    Some(slot) if slot.is_object() && v.is_object() => deep_merge(slot, v),
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, p) => *b = p.clone(),
    }
}

pub fn wildcard_to_regex(pat: &str) -> regex::Regex {
    let mut s = String::from("^");
    for c in pat.chars() {
        match c {
            '*' => s.push_str(".*"),
            '?' => s.push('.'),
            c => s.push_str(&regex::escape(&c.to_string())),
        }
    }
    s.push('$');
    regex::Regex::new(&s).unwrap_or_else(|_| regex::Regex::new("^$").unwrap())
}

/// Convert a JSON document into a tantivy document with both views plus `_source`.
/// Build the tantivy document. Takes the source by value so the JSON tree is
/// moved into the first view instead of deep-copied for both.
/// Apply a normalizer the way OpenSearch does at index time.
pub fn normalize(value: &Value, normalizer: &str) -> Option<Value> {
    let s = value.as_str()?;
    match normalizer {
        "" => Some(Value::String(s.to_string())),
        "lowercase" => Some(Value::String(s.to_lowercase())),
        "uppercase" => Some(Value::String(s.to_uppercase())),
        _ => None,
    }
}


/// Whether a value can be read as the type its mapping declares.
///
/// Only the types with a real parse step are checked; a string field takes
/// whatever it is given.
fn value_is_valid(v: &Value, ty: &str, format: Option<&str>) -> bool {
    match ty {
        "date" | "date_nanos" => canonical_date_with(v, format).is_some() || v.is_number(),
        "ip" => v.as_str().map(|s| canonical_ip(s).is_some()).unwrap_or(false),
        "byte" | "short" | "integer" | "long" | "unsigned_long" | "float" | "half_float"
        | "double" | "scaled_float" => match v {
            Value::Number(_) => true,
            Value::String(s) => s.parse::<f64>().is_ok(),
            _ => false,
        },
        "boolean" => matches!(v, Value::Bool(_))
            || matches!(v.as_str(), Some("true") | Some("false")),
        _ => true,
    }
}

/// Field values that cannot be read as their mapped type.
///
/// A field that says `ignore_malformed` has its bad values dropped and its name
/// recorded; one that does not makes the whole write fail, which is how a
/// field-level `false` overrides an index-wide `true`.
pub fn scan_malformed(
    source: &Value,
    mapping: &Mapping,
    index_default: bool,
) -> std::result::Result<Vec<String>, (String, String)> {
    let mut ignored = Vec::new();
    walk_malformed(source, &mut String::new(), mapping, index_default, &mut ignored)?;
    ignored.sort();
    ignored.dedup();
    Ok(ignored)
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
            let fmt = mapping.field_option(path, "format");
            let fmt = fmt.as_ref().and_then(|v| v.as_str());
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

/// Drop a leaf the index is not going to hold.
pub fn remove_path(node: &mut Value, path: &str) {
    let Some((head, rest)) = path.split_once('.') else {
        if let Some(o) = node.as_object_mut() {
            o.remove(path);
        }
        return;
    };
    if let Some(child) = node.as_object_mut().and_then(|o| o.get_mut(head)) {
        remove_path(child, rest);
    }
}

/// Bring a value in line with the type its mapping declares.
///
/// A client may send `"800.0"` for a field mapped as a float; OpenSearch stores
/// a number there, and queries phrased with a number have to find it.
fn coerce_leaves(node: &mut Value, path: &mut String, mapping: &Mapping) {
    match node {
        Value::Object(obj) => {
            let base = path.len();
            for (k, v) in obj.iter_mut() {
                if base > 0 {
                    path.push('.');
                }
                path.push_str(k);
                coerce_leaves(v, path, mapping);
                path.truncate(base);
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                coerce_leaves(v, path, mapping);
            }
        }
        leaf => {
            let ty = mapping.type_of(path);
            if matches!(ty, Some("date") | Some("date_nanos")) {
                let fmt = mapping.field_option(path, "format");
                let fmt = fmt.as_ref().and_then(|v| v.as_str());
                if let Some(c) = canonical_date_prec(leaf, fmt, ty == Some("date_nanos")) {
                    *leaf = Value::String(c);
                }
            } else if let Some(c) = coerce_leaf(leaf, ty) {
                *leaf = c;
            }
            // a half_float holds sixteen bits, so the value it keeps is the
            // nearest one that fits -- 184.4 becomes 184.375, and a search
            // paging past that number has to see the same figure the index does
            if ty == Some("half_float") {
                if let Some(n) = leaf.as_f64() {
                    if let Some(q) = serde_json::Number::from_f64(half_float(n)) {
                        *leaf = Value::Number(q);
                    }
                }
            }
        }
    }
}

/// The date forms OpenSearch's default `strict_date_optional_time` accepts.
///
/// A bare `2024-08-12` is a date to OpenSearch but not to RFC 3339, and a
/// field indexed as text rather than as a date has no column for a range or an
/// aggregation to read.
pub fn parse_date_lenient(s: &str) -> Option<tantivy::time::OffsetDateTime> {
    use tantivy::time::{Date, Month, OffsetDateTime, Time};
    if let Some(dt) = crate::query::parse_datetime(s) {
        return Some(dt.into_utc());
    }
    if s.contains("||") || s.starts_with("now") {
        return parse_date_math(s).map(|(dt, _)| dt);
    }
    let (day_part, time_part) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t.trim_end_matches('Z'))),
        None => (s, None),
    };
    let nums: Vec<&str> = day_part.split('-').collect();
    if nums.is_empty() || nums.len() > 3 {
        return None;
    }
    let widths = [4usize, 2, 2];
    let mut parts = [1i64, 1, 1];
    for (i, p) in nums.iter().enumerate() {
        if p.len() != widths[i] || !p.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        parts[i] = p.parse().ok()?;
    }
    let date = Date::from_calendar_date(
        parts[0] as i32,
        Month::try_from(parts[1] as u8).ok()?,
        parts[2] as u8,
    )
    .ok()?;
    let time = match time_part {
        None => Time::MIDNIGHT,
        Some(t) => {
            let (hms, frac) = match t.split_once('.') {
                Some((a, b)) => (a, b),
                None => (t, ""),
            };
            let f: Vec<&str> = hms.split(':').collect();
            if f.is_empty() || f.len() > 3 {
                return None;
            }
            let mut c = [0u32; 3];
            for (i, p) in f.iter().enumerate() {
                c[i] = p.parse().ok()?;
            }
            // the fraction is kept whole: a date reports milliseconds and
            // a date_nanos the nanoseconds, and which one this is has not
            // been decided yet here
            let nanos: u32 = if frac.is_empty() {
                0
            } else {
                let mut d = frac.trim_end_matches(|c: char| !c.is_ascii_digit()).to_string();
                d.truncate(9);
                while d.len() < 9 {
                    d.push('0');
                }
                d.parse().ok()?
            };
            Time::from_hms_nano(c[0] as u8, c[1] as u8, c[2] as u8, nanos).ok()?
        }
    };
    Some(OffsetDateTime::new_utc(date, time))
}

/// The window a date column can hold. Nanoseconds in an i64 reach about 292
/// years either side of the epoch, so an open-ended range is filled to the
/// edges of that rather than to a year the column could not represent.
const DATE_FLOOR: &str = "1700-01-01T00:00:00.000Z";
const DATE_CEIL: &str = "2250-01-01T00:00:00.000Z";

/// A range field written with only one end is open at the other, which a
/// comparison against a missing sub-field cannot express. The open side is
/// filled with the extreme its type allows, in the indexing view only.
fn fill_open_ranges(out: &mut Value, mapping: &Mapping) {
    let ranges: Vec<(String, String)> = mapping
        .types
        .iter()
        .filter(|(_, t)| t.ends_with("_range"))
        .map(|(p, t)| (p.clone(), t.clone()))
        .collect();
    for (path, ty) in ranges {
        let pointer = format!("/{}", path.replace('.', "/"));
        let dated = ty.starts_with("date");
        let Some(node) = out.pointer_mut(&pointer).and_then(|n| n.as_object_mut()) else {
            continue;
        };
        if dated {
            for key in ["gte", "gt", "lte", "lt"] {
                if let Some(v) = node.get(key) {
                    if let Some(c) = canonical_date(v) {
                        node.insert(key.into(), Value::String(c));
                    }
                }
            }
        }
        // comparisons run against `gte`/`lte`, so an exclusive endpoint is
        // moved one step inward rather than left in a form nothing reads
        let step = |v: &Value, forward: bool| -> Option<Value> {
            if dated {
                let dt = parse_date_lenient(v.as_str()?)?;
                let shifted = if forward {
                    dt + tantivy::time::Duration::milliseconds(1)
                } else {
                    dt - tantivy::time::Duration::milliseconds(1)
                };
                return Some(Value::String(format_utc_millis(shifted)));
            }
            // a whole-number range steps by one; a fractional one has no next
            // value to move to, so the bound is kept as written
            let n = v.as_i64()?;
            Some(Value::from(if forward { n + 1 } else { n - 1 }))
        };
        for (from, to, forward) in [("gt", "gte", true), ("lt", "lte", false)] {
            if node.contains_key(to) {
                continue;
            }
            let Some(v) = node.get(from).cloned() else { continue };
            let moved = step(&v, forward).unwrap_or(v);
            node.insert(to.into(), moved);
        }
        let has_lower = node.contains_key("gte");
        let has_upper = node.contains_key("lte");
        if !has_lower {
            node.insert(
                "gte".into(),
                if dated {
                    Value::String(DATE_FLOOR.into())
                } else {
                    serde_json::json!(f64::MIN)
                },
            );
        }
        if !has_upper {
            node.insert(
                "lte".into(),
                if dated {
                    Value::String(DATE_CEIL.into())
                } else {
                    serde_json::json!(f64::MAX)
                },
            );
        }
    }
}

/// A flat_object is queryable by its own name, which means every value beneath
/// it has to live somewhere addressable. They are gathered into one list
/// alongside, in the indexing view only.
fn gather_flat_objects(out: &mut Value, mapping: &Mapping) {
    let flats: Vec<String> = mapping
        .types
        .iter()
        .filter(|(_, t)| t.as_str() == "flat_object")
        .map(|(p, _)| p.clone())
        .collect();
    let Some(obj) = out.as_object_mut() else { return };
    for path in flats {
        let pointer = format!("/{}", path.replace('.', "/"));
        let Some(node) = obj.get(path.split('.').next().unwrap_or(&path)) else { continue };
        let root = Value::Object(obj.clone());
        let Some(node) = root.pointer(&pointer).or(Some(node)) else { continue };
        let mut values = Vec::new();
        collect_leaves(node, &mut values);
        if values.is_empty() {
            continue;
        }
        obj.insert(format!("{path}.{FLAT_VALUES}"), Value::Array(values));
    }
}

fn collect_leaves(node: &Value, out: &mut Vec<Value>) {
    match node {
        Value::Object(o) => o.values().for_each(|v| collect_leaves(v, out)),
        Value::Array(a) => a.iter().for_each(|v| collect_leaves(v, out)),
        Value::Null => {}
        leaf => out.push(leaf.clone()),
    }
}

/// The type OpenSearch infers for a value before any template is consulted.
fn json_mapping_type(v: &Value) -> &'static str {
    match v {
        Value::Object(_) => "object",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_f64() && n.as_i64().is_none() {
                "double"
            } else {
                "long"
            }
        }
        Value::String(s) => {
            if s.len() >= 10
                && s.as_bytes()[4] == b'-'
                && s.as_bytes()[7] == b'-'
                && parse_date_lenient(s).is_some()
            {
                "date"
            } else {
                "string"
            }
        }
        Value::Array(a) => a.first().map(json_mapping_type).unwrap_or("string"),
        Value::Null => "string",
    }
}

/// `date_*` against a field name -- the only wildcard a template `match` uses.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let mut rest = name;
    let mut parts = pattern.split('*').peekable();
    let first = parts.next().unwrap_or("");
    if !rest.starts_with(first) {
        return false;
    }
    rest = &rest[first.len()..];
    if !pattern.contains('*') {
        return rest.is_empty();
    }
    while let Some(part) = parts.next() {
        if part.is_empty() {
            if parts.peek().is_none() {
                return true;
            }
            continue;
        }
        if parts.peek().is_none() {
            return rest.ends_with(part);
        }
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    true
}

/// Where a flat_object field's values are gathered so the field itself can be
/// queried without naming a path inside it.
pub const FLAT_VALUES: &str = "_obs_values";

/// How many tokens a standard analyser would find.
pub fn token_count(text: &str) -> u64 {
    text.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).count() as u64
}

/// `2019-12-15||/d`, `now-1d`, `now+1M/M`: an anchor followed by shifts and a
/// rounding, which is how OpenSearch writes a date relative to another.
/// Resolve a date-math index name into the name it stands for.
///
/// `<logstash-{now/M}>` names the index for the current month; the braces hold
/// a date expression and, after a pipe, how to write it.
pub fn resolve_date_math_name(name: &str) -> String {
    let Some(inner) = name.strip_prefix('<').and_then(|s| s.strip_suffix('>')) else {
        return name.to_string();
    };
    let mut out = String::new();
    let mut rest = inner;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let body = &rest[open + 1..open + close];
        rest = &rest[open + close + 1..];
        // the expression may name its own format after a pipe
        let (expr, fmt) = match body.split_once('|') {
            Some((e, f)) => (e, f),
            None => (body, "yyyy.MM.dd"),
        };
        match parse_date_math(expr.trim()) {
            Some((d, _)) => out.push_str(&format_with_pattern(d, fmt.trim())),
            None => out.push_str(body),
        }
    }
    out.push_str(rest);
    out
}

/// Write a date the way a Java-style pattern asks for.
/// A moment written the way a named or literal date format asks for.
pub fn format_millis(ms: i64, format: &str) -> Option<String> {
    format_millis_at(ms, format, 0)
}

/// The same, written in a zone rather than in UTC.
pub fn format_millis_at(ms: i64, format: &str, zone_ms: i64) -> Option<String> {
    if zone_ms != 0 {
        let local = tantivy::time::OffsetDateTime::from_unix_timestamp_nanos(
            (ms + zone_ms) as i128 * 1_000_000,
        )
        .ok()?;
        let total = zone_ms / 60_000;
        let sign = if total < 0 { '-' } else { '+' };
        let total = total.abs();
        let body = match format {
            "iso8601" | "strict_date_optional_time" | "date_optional_time" | "date_time"
            | "strict_date_time" => format!(
                "{}.{:03}",
                format_with_pattern(local, "yyyy-MM-dd'T'HH:mm:ss").replace('\'', ""),
                local.millisecond()
            ),
            other => return format_millis_utc(ms + zone_ms, other),
        };
        return Some(format!("{body}{sign}{:02}:{:02}", total / 60, total % 60));
    }
    format_millis_utc(ms, format)
}

fn format_millis_utc(ms: i64, format: &str) -> Option<String> {
    let dt = tantivy::time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .ok()?;
    Some(match format {
        "epoch_millis" => ms.to_string(),
        "epoch_second" => (ms / 1000).to_string(),
        "strict_date" | "date" | "yyyy-MM-dd" => format_with_pattern(dt, "yyyy-MM-dd"),
        "basic_date" => format_with_pattern(dt, "yyyyMMdd"),
        "iso8601" | "strict_date_optional_time" | "date_optional_time" | "date_time"
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

fn format_with_pattern(d: tantivy::time::OffsetDateTime, pattern: &str) -> String {
    let mut out = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        let mut run = 1;
        while chars.peek() == Some(&c) {
            chars.next();
            run += 1;
        }
        match c {
            'y' => out.push_str(&format!("{:0run$}", d.year(), run = run)),
            'M' => out.push_str(&format!("{:0run$}", d.month() as u8, run = run)),
            'd' => out.push_str(&format!("{:0run$}", d.day(), run = run)),
            'H' => out.push_str(&format!("{:0run$}", d.hour(), run = run)),
            'm' => out.push_str(&format!("{:0run$}", d.minute(), run = run)),
            's' => out.push_str(&format!("{:0run$}", d.second(), run = run)),
            other => {
                for _ in 0..run {
                    out.push(other);
                }
            }
        }
    }
    out
}

fn parse_date_math(s: &str) -> Option<(tantivy::time::OffsetDateTime, Option<char>)> {
    use tantivy::time::{Duration, OffsetDateTime};
    let (anchor, ops) = match s.split_once("||") {
        Some((a, o)) => (parse_date_lenient(a)?, o),
        None => (OffsetDateTime::now_utc(), s.strip_prefix("now")?),
    };
    let mut dt = anchor;
    let mut rounded = None;
    let mut rest = ops;
    while !rest.is_empty() {
        let (op, tail) = rest.split_at(1);
        match op {
            "/" => {
                let (unit, tail) = tail.split_at(1.min(tail.len()));
                dt = round_down(dt, unit)?;
                rounded = unit.chars().next();
                rest = tail;
            }
            "+" | "-" => {
                let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
                let tail = &tail[digits.len()..];
                let (unit, tail) = tail.split_at(1.min(tail.len()));
                let n: i64 = if digits.is_empty() { 1 } else { digits.parse().ok()? };
                let n = if op == "-" { -n } else { n };
                dt = match unit {
                    "y" => shift_months(dt, n * 12)?,
                    "M" => shift_months(dt, n)?,
                    "w" => dt + Duration::days(n * 7),
                    "d" => dt + Duration::days(n),
                    "H" | "h" => dt + Duration::hours(n),
                    "m" => dt + Duration::minutes(n),
                    "s" => dt + Duration::seconds(n),
                    _ => return None,
                };
                rest = tail;
            }
            _ => return None,
        }
    }
    Some((dt, rounded))
}

/// A rounded date math expression names a whole unit, not an instant. Which
/// end of it a bound means depends on the bound: `gt: .../d` excludes the whole
/// day, `gte: .../d` includes it from the start.
pub fn canonical_date_bound(v: &Value, round_up: bool) -> Option<String> {
    let Some(s) = v.as_str() else { return canonical_date(v) };
    if !round_up || !(s.contains("||") || s.starts_with("now")) {
        return canonical_date(v);
    }
    let (dt, unit) = parse_date_math(s)?;
    let Some(unit) = unit else { return canonical_date(v) };
    // the last instant the unit covers
    let end = advance_unit(dt, unit)? - tantivy::time::Duration::milliseconds(1);
    canonical_date(&Value::String(format_utc_millis(end)))
}

fn advance_unit(
    dt: tantivy::time::OffsetDateTime,
    unit: char,
) -> Option<tantivy::time::OffsetDateTime> {
    use tantivy::time::Duration;
    Some(match unit {
        'y' => shift_months(dt, 12)?,
        'M' => shift_months(dt, 1)?,
        'w' => dt + Duration::days(7),
        'd' => dt + Duration::days(1),
        'H' | 'h' => dt + Duration::hours(1),
        'm' => dt + Duration::minutes(1),
        's' => dt + Duration::seconds(1),
        _ => return None,
    })
}

fn format_utc_millis(dt: tantivy::time::OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.millisecond(),
    )
}

fn round_down(
    dt: tantivy::time::OffsetDateTime,
    unit: &str,
) -> Option<tantivy::time::OffsetDateTime> {
    use tantivy::time::{Date, Duration, Month, Time};
    let midnight = |d: Date| d.with_time(Time::MIDNIGHT).assume_utc();
    Some(match unit {
        "y" => midnight(Date::from_calendar_date(dt.year(), Month::January, 1).ok()?),
        "M" => midnight(Date::from_calendar_date(dt.year(), dt.month(), 1).ok()?),
        "w" => {
            let back = dt.weekday().number_days_from_monday() as i64;
            midnight(dt.date() - Duration::days(back))
        }
        "d" => midnight(dt.date()),
        "H" | "h" => dt.replace_minute(0).ok()?.replace_second(0).ok()?.replace_nanosecond(0).ok()?,
        "m" => dt.replace_second(0).ok()?.replace_nanosecond(0).ok()?,
        "s" => dt.replace_nanosecond(0).ok()?,
        _ => return None,
    })
}

fn shift_months(
    dt: tantivy::time::OffsetDateTime,
    n: i64,
) -> Option<tantivy::time::OffsetDateTime> {
    use tantivy::time::{Date, Month};
    let total = dt.year() as i64 * 12 + (dt.month() as i64 - 1) + n;
    let (y, m) = (total.div_euclid(12) as i32, total.rem_euclid(12) as u8 + 1);
    let month = Month::try_from(m).ok()?;
    let day = dt.day().min(days_in_month(y, month));
    Some(Date::from_calendar_date(y, month, day).ok()?.with_time(dt.time()).assume_utc())
}

fn days_in_month(year: i32, month: tantivy::time::Month) -> u8 {
    use tantivy::time::Month::*;
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

/// Re-format a stored date string with milliseconds, which is how a key is
/// written back out.
pub fn canonical_date_str(s: &str) -> Option<String> {
    canonical_date(&Value::String(s.to_string()))
}

/// A date in the one spelling the index holds.
pub fn canonical_date(v: &Value) -> Option<String> {
    canonical_date_with(v, None)
}

/// As `canonical_date`, but honouring the `format` a mapping declares.
///
/// A bare number is epoch milliseconds unless the field says otherwise, which
/// is the assumption OpenSearch makes too.
pub fn canonical_date_with(v: &Value, format: Option<&str>) -> Option<String> {
    canonical_date_prec(v, format, false)
}

/// As `canonical_date_with`, but able to keep the whole fraction.
///
/// A `date` reports milliseconds and a `date_nanos` reports nanoseconds; the
/// finer resolution is the only reason the second type exists, so truncating
/// on the way in would throw away what it was chosen for.
pub fn canonical_date_prec(v: &Value, format: Option<&str>, nanos: bool) -> Option<String> {
    let scale: i128 = match format {
        Some(f) if f.contains("epoch_second") => 1_000_000_000,
        _ => 1_000_000,
    };
    let dt = match v {
        Value::Number(n) => tantivy::time::OffsetDateTime::from_unix_timestamp_nanos(
            (n.as_f64()? as i128) * scale,
        )
        .ok()?,
        Value::String(s) => match s.parse::<f64>() {
            // a number written as text still means what the format says
            Ok(n) if format.is_some() => {
                tantivy::time::OffsetDateTime::from_unix_timestamp_nanos((n as i128) * scale).ok()?
            }
            // `2019` is a year before it is a count of milliseconds, so the
            // date reading is tried first and the epoch only where nothing
            // else could be read from the digits
            Ok(n) => parse_date_lenient(s).or_else(|| {
                tantivy::time::OffsetDateTime::from_unix_timestamp_nanos((n as i128) * scale).ok()
            })?,
            _ => parse_date_lenient(s)?,
        },
        _ => return None,
    };
    if nanos {
        return Some(format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}Z",
            dt.year(),
            dt.month() as u8,
            dt.day(),
            dt.hour(),
            dt.minute(),
            dt.second(),
            dt.nanosecond(),
        ));
    }
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.millisecond(),
    ))
}

/// The span a CIDR block covers, written the way a range is: the first address
/// it contains, and the first one it does not.
pub fn cidr_bounds(mask: &str) -> Option<(String, String)> {
    let (lo, hi) = canonical_cidr(mask)?;
    let mut octets = [0u8; 16];
    for (i, o) in octets.iter_mut().enumerate() {
        *o = u8::from_str_radix(&hi[i * 2..i * 2 + 2], 16).ok()?;
    }
    // one past the last address in the block
    for byte in octets.iter_mut().rev() {
        match byte.checked_add(1) {
            Some(next) => {
                *byte = next;
                break;
            }
            None => *byte = 0,
        }
    }
    let past: String = octets.iter().map(|b| format!("{b:02x}")).collect();
    Some((ip_from_canonical(&lo)?, ip_from_canonical(&past)?))
}

/// An IP in a form that sorts the way addresses do.
///
/// Text comparison puts "192.168.0.10" below "192.168.0.9", so ranges and
/// subnet queries need the fixed-width binary form. IPv4 is widened to its
/// IPv6-mapped shape so both families share one ordering.
pub fn canonical_ip(s: &str) -> Option<String> {
    let octets = match s.parse::<std::net::IpAddr>().ok()? {
        std::net::IpAddr::V4(v) => v.to_ipv6_mapped().octets(),
        std::net::IpAddr::V6(v) => v.octets(),
    };
    Some(octets.iter().map(|b| format!("{b:02x}")).collect())
}

/// Read an address back out of the fixed-width form it is stored in.
pub fn ip_from_canonical(hex: &str) -> Option<String> {
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut octets = [0u8; 16];
    for (i, o) in octets.iter_mut().enumerate() {
        *o = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    let addr = std::net::Ipv6Addr::from(octets);
    Some(match addr.to_ipv4_mapped() {
        Some(v4) => v4.to_string(),
        None => addr.to_string(),
    })
}

/// The first and last address of a CIDR block, canonicalised.
pub fn canonical_cidr(s: &str) -> Option<(String, String)> {
    let (addr, bits) = s.split_once('/')?;
    let bits: u32 = bits.trim().parse().ok()?;
    let (mut lo, family_bits) = match addr.trim().parse::<std::net::IpAddr>().ok()? {
        std::net::IpAddr::V4(v) => (v.to_ipv6_mapped().octets(), 32u32),
        std::net::IpAddr::V6(v) => (v.octets(), 128u32),
    };
    if bits > family_bits {
        return None;
    }
    // an IPv4 prefix addresses the low 32 bits of the mapped form
    let prefix = bits + (128 - family_bits);
    let mut hi = lo;
    for i in 0..16u32 {
        let keep = prefix.saturating_sub(i * 8).min(8);
        let mask = if keep == 0 { 0u8 } else { (!0u8) << (8 - keep) };
        lo[i as usize] &= mask;
        hi[i as usize] |= !mask;
    }
    let hex = |o: [u8; 16]| -> String { o.iter().map(|b| format!("{b:02x}")).collect() };
    Some((hex(lo), hex(hi)))
}

fn coerce_leaf(v: &Value, ty: Option<&str>) -> Option<Value> {
    if matches!(ty, Some("date") | Some("date_nanos")) {
        return canonical_date(v).map(Value::String);
    }
    let s = v.as_str()?;
    match ty? {
        // a token_count field holds how many tokens the text produced, not
        // the text itself
        "token_count" => Some(Value::from(token_count(s))),
        "byte" | "short" | "integer" | "long" | "unsigned_long" => {
            // an integer field takes the whole part of a decimal, and the
            // magnitudes unsigned_long reaches do not survive a trip via f64
            let whole = s.split_once('.').map(|(a, _)| a).unwrap_or(s);
            whole
                .parse::<i64>()
                .ok()
                .map(Value::from)
                .or_else(|| whole.parse::<u64>().ok().map(Value::from))
        }
        "float" | "half_float" | "double" | "scaled_float" => {
            s.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(Value::Number)
        }
        "ip" => canonical_ip(s).map(Value::String),
        "boolean" => match s {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        _ => None,
    }
}

/// Add the normalized copies a mapping's multi-fields ask for. These only go
/// into the index; `_source` is always what the client sent.
///
/// The copy is added as a dotted top-level key, which the JSON fields expand
/// into the same path a nested object would produce -- and unlike nesting, it
/// does not collide with the parent being a scalar.
pub fn expand_for_indexing(source: &Value, mapping: &Mapping) -> Value {
    let subs = mapping.normalized_subfields();
    let mut out = source.clone();
    coerce_leaves(&mut out, &mut String::new(), mapping);
    fill_open_ranges(&mut out, mapping);
    gather_flat_objects(&mut out, mapping);
    if subs.is_empty() {
        return out;
    }
    let source = &out.clone();
    let Some(obj) = out.as_object_mut() else { return out };
    for (parent, sub, normalizer) in subs {
        let Some(v) = source.pointer(&format!("/{}", parent.replace('.', "/"))).cloned() else {
            continue;
        };
        let normalized = match &v {
            Value::Array(items) => {
                let mapped: Vec<Value> =
                    items.iter().filter_map(|x| normalize(x, &normalizer)).collect();
                if mapped.is_empty() {
                    continue;
                }
                Value::Array(mapped)
            }
            other => match normalize(other, &normalizer) {
                Some(n) => n,
                None => continue,
            },
        };
        obj.insert(format!("{parent}.{sub}"), normalized);
    }
    out
}

pub fn make_doc(fields: &Fields, id: &str, source: Value, raw: &str, seq: u64) -> TantivyDocument {
    let mut d = TantivyDocument::default();
    d.add_text(fields.id, id);
    d.add_text(fields.source, raw);
    d.add_u64(fields.seq, seq);
    if let Value::Object(obj) = source {
        let converted: BTreeMap<String, OwnedValue> =
            obj.into_iter().map(|(k, v)| (k, OwnedValue::from(v))).collect();
        d.add_object(fields.dynamic, converted.clone());
        d.add_object(fields.raw, converted);
    }
    d
}
