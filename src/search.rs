//! The search path: query execution, hit assembly, sorting and aggregations.
//!
//! Aggregation requests are handed to tantivy almost untouched -- its
//! aggregation JSON already matches OpenSearch's -- after rewriting each
//! `field` onto the JSON view that backs it.

use crate::api::{Params, apply_source_selector, err, err_caused_by, no_such_index};
use crate::query::{Ctx, View};
use crate::store::{IdxState, Store};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};
use std::cmp::Ordering;
use tantivy::aggregation::AggContextParams;
use tantivy::aggregation::agg_req::Aggregations;
use tantivy::aggregation::DistributedAggregationCollector;
use tantivy::aggregation::intermediate_agg_result::IntermediateAggregationResults;
use tantivy::collector::{Count, TopDocs};
use tantivy::schema::{OwnedValue, Value as _};
use tantivy::{DocAddress, Searcher, TantivyDocument};

const DEFAULT_TRACK_TOTAL_HITS: u64 = 10_000;

#[derive(Clone, Debug, PartialEq)]
enum SortValue {
    I64(i64),
    U64(u64),
    F64(f64),
    Str(String),
    Missing,
}

impl SortValue {
    fn as_f64(&self) -> Option<f64> {
        match self {
            SortValue::I64(v) => Some(*v as f64),
            SortValue::U64(v) => Some(*v as f64),
            SortValue::F64(v) => Some(*v),
            _ => None,
        }
    }
}

impl SortValue {
    fn cmp_asc(&self, other: &SortValue) -> Ordering {
        match (self, other) {
            // a missing value always sorts last, whatever the direction
            (SortValue::Missing, SortValue::Missing) => Ordering::Equal,
            (SortValue::Missing, _) => Ordering::Greater,
            (_, SortValue::Missing) => Ordering::Less,
            (SortValue::I64(a), SortValue::I64(b)) => a.cmp(b),
            (SortValue::U64(a), SortValue::U64(b)) => a.cmp(b),
            (SortValue::Str(a), SortValue::Str(b)) => a.cmp(b),
            (SortValue::Str(_), _) => Ordering::Greater,
            (_, SortValue::Str(_)) => Ordering::Less,
            (a, b) => a
                .as_f64()
                .partial_cmp(&b.as_f64())
                .unwrap_or(Ordering::Equal),
        }
    }

    /// A date sort value is stored in nanoseconds and reported in
    /// milliseconds, which is the unit the field was given in.
    fn to_json_scaled(&self, is_date: bool) -> Value {
        if !is_date {
            return self.to_json();
        }
        match self {
            SortValue::I64(v) => json!(v / 1_000_000),
            SortValue::U64(v) => json!(v / 1_000_000),
            SortValue::F64(n) => json!((*n / 1e6) as i64),
            other => other.to_json(),
        }
    }

    fn to_json(&self) -> Value {
        match self {
            SortValue::I64(v) => json!(v),
            SortValue::U64(v) => json!(v),
            SortValue::F64(n) => {
                if n.fract() == 0.0 && n.abs() < 9e15 {
                    json!(*n as i64)
                } else {
                    json!(n)
                }
            }
            SortValue::Str(s) => json!(s),
            SortValue::Missing => Value::Null,
        }
    }
}

struct SortKey {
    field: String,
    desc: bool,
    mode: Option<String>,
}

fn parse_sort(spec: Option<&Value>) -> Vec<SortKey> {
    let Some(spec) = spec else { return vec![] };
    let items: Vec<Value> = match spec {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    };
    let mut out = Vec::new();
    for item in items {
        match item {
            Value::String(f) => out.push(SortKey { field: f, desc: false, mode: None }),
            Value::Object(o) => {
                for (field, opts) in o {
                    let desc = match &opts {
                        Value::String(s) => s.eq_ignore_ascii_case("desc"),
                        Value::Object(oo) => oo
                            .get("order")
                            .and_then(|v| v.as_str())
                            .map(|s| s.eq_ignore_ascii_case("desc"))
                            .unwrap_or(false),
                        _ => false,
                    };
                    let mode = opts
                        .get("mode")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_ascii_lowercase());
                    out.push(SortKey { field, desc, mode });
                }
            }
            _ => {}
        }
    }
    out
}

/// Per-segment readers for one sort field, opened once and reused.
struct SortColumns {
    per_segment: Vec<(Option<tantivy::columnar::StrColumn>, Option<tantivy::columnar::Column<u64>>, Option<tantivy::columnar::ColumnType>)>,
}

/// Decode one raw columnar u64 into the value its column type really holds.
fn decode_col_value(raw: u64, ty: tantivy::columnar::ColumnType) -> Option<SortValue> {
    use tantivy::columnar::ColumnType;
    match ty {
        ColumnType::I64 | ColumnType::DateTime => Some(SortValue::I64(
            tantivy::columnar::MonotonicallyMappableToU64::from_u64(raw),
        )),
        ColumnType::F64 => Some(SortValue::F64(
            <f64 as tantivy::columnar::MonotonicallyMappableToU64>::from_u64(raw),
        )),
        ColumnType::U64 => Some(SortValue::U64(raw)),
        ColumnType::Bool => Some(SortValue::I64(raw as i64)),
        ColumnType::Str | ColumnType::Bytes | ColumnType::IpAddr => None,
    }
}

impl SortColumns {
    /// Open the readers for a single segment.
    fn for_segment(reader: &tantivy::SegmentReader, column: &str) -> SortColumns {
        let ff = reader.fast_fields();
        let str_col = ff.str(column).ok().flatten();
        let (num_col, ty) = match ff.u64_lenient(column) {
            Ok(Some((c, t))) => (Some(c), Some(t)),
            _ => (None, None),
        };
        SortColumns { per_segment: vec![(str_col, num_col, ty)] }
    }

    /// Every numeric value a document holds for this column.
    fn numeric_values(&self, doc: tantivy::DocId) -> Vec<f64> {
        let Some((_, num, ty)) = self.per_segment.first() else { return Vec::new() };
        let (Some(col), Some(ty)) = (num, ty) else { return Vec::new() };
        col.values_for_doc(doc)
            .filter_map(|raw| decode_col_value(raw, *ty).and_then(|v| v.as_f64()))
            .collect()
    }

    /// Read the value for a document inside the segment this was opened for.
    fn read(&self, doc: tantivy::DocId, desc: bool, mode: Option<&str>) -> SortValue {
        self.value(DocAddress::new(0, doc), desc, mode)
    }

    fn value(&self, addr: DocAddress, desc: bool, mode: Option<&str>) -> SortValue {
        let mode = mode.unwrap_or(if desc { "max" } else { "min" });
        let Some((str_col, num_col, ty)) = self.per_segment.get(addr.segment_ord as usize) else {
            return SortValue::Missing;
        };
        if let (Some(num), Some(ty)) = (num_col, ty) {
            let mut vals: Vec<SortValue> = Vec::new();
            for raw in num.values_for_doc(addr.doc_id) {
                use tantivy::columnar::ColumnType;
                let decoded = match ty {
                    ColumnType::I64 | ColumnType::DateTime => Some(SortValue::I64(
                        tantivy::columnar::MonotonicallyMappableToU64::from_u64(raw),
                    )),
                    ColumnType::F64 => Some(SortValue::F64(
                        <f64 as tantivy::columnar::MonotonicallyMappableToU64>::from_u64(raw),
                    )),
                    ColumnType::U64 => Some(SortValue::U64(raw)),
                    ColumnType::Bool => Some(SortValue::I64(raw as i64)),
                    ColumnType::Str | ColumnType::Bytes | ColumnType::IpAddr => None,
                };
                if let Some(d) = decoded {
                    vals.push(d);
                }
            }
            if !vals.is_empty() {
                return reduce_sort_values(&mut vals, mode);
            }
        }
        if let Some(sc) = str_col {
            if let Some(ord) = sc.term_ords(addr.doc_id).next() {
                let mut buf = Vec::new();
                if sc.ord_to_bytes(ord, &mut buf).unwrap_or(false) {
                    if let Ok(s) = String::from_utf8(buf) {
                        return SortValue::Str(s);
                    }
                }
            }
        }
        SortValue::Missing
    }
}

/// Collapse a document's multiple values for one sort field.
///
/// OpenSearch does this with Java `long` arithmetic, so `sum` and `avg` wrap on
/// overflow -- `[i64::MAX, 1]` really does sort as a large negative number --
/// while `median` averages the two middle values in floating point.
fn reduce_sort_values(vals: &mut Vec<SortValue>, mode: &str) -> SortValue {
    let n = vals.len();
    match mode {
        "max" | "min" => {
            vals.sort_by(|a, b| a.cmp_asc(b));
            if mode == "max" { vals[n - 1].clone() } else { vals[0].clone() }
        }
        "sum" | "avg" => {
            // doc values arrive sorted in OpenSearch, so summing in ascending
            // order is what reproduces its floating-point result exactly
            vals.sort_by(|a, b| a.cmp_asc(b));
            let is_float = vals.iter().any(|v| matches!(v, SortValue::F64(_)));
            if is_float {
                let total: f64 = vals.iter().filter_map(|v| v.as_f64()).sum();
                let out = if mode == "sum" { total } else { total / n as f64 };
                SortValue::F64(out)
            } else if vals.iter().any(|v| matches!(v, SortValue::U64(_))) {
                // one unsigned value makes the whole field unsigned
                let total = vals.iter().fold(0u64, |acc, v| match v {
                    SortValue::U64(x) => acc.wrapping_add(*x),
                    SortValue::I64(x) => acc.wrapping_add(*x as u64),
                    _ => acc,
                });
                if mode == "sum" {
                    SortValue::U64(total)
                } else {
                    // unsigned_long keeps exact integer arithmetic, rounding half up
                    SortValue::U64((total + (n as u64) / 2) / n as u64)
                }
            } else {
                let total = vals.iter().fold(0i64, |acc, v| match v {
                    SortValue::I64(x) => acc.wrapping_add(*x),
                    SortValue::U64(x) => acc.wrapping_add(*x as i64),
                    _ => acc,
                });
                if mode == "sum" {
                    SortValue::I64(total)
                } else {
                    SortValue::I64((total as f64 / n as f64).round() as i64)
                }
            }
        }
        "median" => {
            vals.sort_by(|a, b| a.cmp_asc(b));
            if n % 2 == 1 {
                return vals[n / 2].clone();
            }
            if vals.iter().any(|v| matches!(v, SortValue::U64(_))) {
                // same exact, wrapping arithmetic the unsigned_long avg uses
                let as_u64 = |v: &SortValue| match v {
                    SortValue::U64(x) => *x,
                    SortValue::I64(x) => *x as u64,
                    other => other.as_f64().unwrap_or(0.0) as u64,
                };
                let lo = as_u64(&vals[n / 2 - 1]);
                let hi = as_u64(&vals[n / 2]);
                return SortValue::U64((lo.wrapping_add(hi) + 1) / 2);
            }
            let lo = vals[n / 2 - 1].as_f64().unwrap_or(0.0);
            let hi = vals[n / 2].as_f64().unwrap_or(0.0);
            let avg = (lo + hi) / 2.0;
            if vals.iter().any(|v| matches!(v, SortValue::F64(_))) {
                return SortValue::F64(avg);
            }
            // integer fields round half up, the way Java's Math.round does
            let rounded = (avg + 0.5).floor();
            if rounded >= 0.0 && vals.iter().any(|v| matches!(v, SortValue::U64(_))) {
                SortValue::U64(rounded as u64)
            } else {
                SortValue::I64(rounded as i64)
            }
        }
        _ => vals[0].clone(),
    }
}

struct Hit {
    shard_idx: usize,
    index: String,
    id: String,
    score: f32,
    source: Value,
    sort: Vec<SortValue>,
    version: u64,
    ignored: Option<Value>,
    seq: u64,
}

/// What one sort key reads out of a segment.
#[derive(Clone)]
enum SortSource {
    Score,
    Doc,
    Column { name: String, desc: bool, mode: Option<String> },
}

/// Top-K collector that evaluates the sort keys while collecting, so a query
/// matching millions of documents still only ever holds K candidates.
struct SortCollector {
    sources: Vec<SortSource>,
    desc: Vec<bool>,
    limit: usize,
    /// Where a previous page ended. Documents up to and including it are not
    /// collected at all, so the page limit applies to what comes after.
    after: Option<Vec<SortValue>>,
}

struct SortSegmentCollector {
    segment_ord: u32,
    sources: Vec<SortSource>,
    columns: Vec<Option<SortColumns>>,
    desc: Vec<bool>,
    limit: usize,
    after: Option<Vec<SortValue>>,
    buf: Vec<Cand>,
    /// The worst candidate currently kept. A document whose leading sort value
    /// cannot beat it is dropped before anything is allocated for it.
    cutoff: Option<SortValue>,
    /// Set when the whole sort is one numeric column, which lets a block of
    /// documents be read from the columnar in one call instead of one at a time.
    block: Option<tantivy::columnar::ColumnBlockAccessor<u64>>,
}

fn cmp_sorted(a: &[SortValue], b: &[SortValue], desc: &[bool]) -> Ordering {
    for (i, d) in desc.iter().enumerate() {
        let ord = a[i].cmp_asc(&b[i]);
        let ord = if *d && ord != Ordering::Equal { ord.reverse() } else { ord };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

fn prune_by(buf: &mut Vec<Cand>, limit: usize, desc: &[bool]) {
    if limit == 0 || buf.len() <= limit {
        return;
    }
    buf.select_nth_unstable_by(limit - 1, |x, y| cmp_sorted(&x.sort, &y.sort, desc));
    buf.truncate(limit);
}

impl tantivy::collector::Collector for SortCollector {
    type Fruit = Vec<Cand>;
    type Child = SortSegmentCollector;

    fn for_segment(
        &self,
        segment_ord: u32,
        reader: &tantivy::SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        let columns: Vec<Option<SortColumns>> = self
            .sources
            .iter()
            .map(|src| match src {
                SortSource::Column { name, .. } => Some(SortColumns::for_segment(reader, name)),
                _ => None,
            })
            .collect();
        // OBSEARCH_NO_BLOCK_SORT=1 disables the vectorised path, for A/B runs
        let single_numeric = std::env::var("OBSEARCH_NO_BLOCK_SORT").is_err()
            && self.sources.len() == 1
            && matches!(self.sources[0], SortSource::Column { ref mode, .. } if mode.is_none())
            && columns
                .first()
                .and_then(|c| c.as_ref())
                .map(|c| {
                    let (str_col, num, _) = &c.per_segment[0];
                    // a multi-valued column would yield several rows per doc,
                    // which the block path has no way to reduce
                    num.as_ref()
                        .map(|n| n.index.get_cardinality().is_full())
                        .unwrap_or(false)
                        && str_col.is_none()
                })
                .unwrap_or(false);
        Ok(SortSegmentCollector {
            segment_ord,
            sources: self.sources.clone(),
            columns,
            desc: self.desc.clone(),
            limit: self.limit,
            after: self.after.clone(),
            buf: Vec::with_capacity(self.limit.saturating_mul(4).max(512).min(4096)),
            cutoff: None,
            block: if single_numeric { Some(Default::default()) } else { None },
        })
    }

    fn requires_scoring(&self) -> bool {
        self.sources.iter().any(|s| matches!(s, SortSource::Score))
    }

    fn merge_fruits(&self, children: Vec<Vec<Cand>>) -> tantivy::Result<Self::Fruit> {
        let mut out: Vec<Cand> = children.into_iter().flatten().collect();
        prune_by(&mut out, self.limit, &self.desc);
        Ok(out)
    }
}

impl SortSegmentCollector {
    fn read_key(&self, i: usize, doc: tantivy::DocId, score: tantivy::Score) -> SortValue {
        match &self.sources[i] {
            SortSource::Score => SortValue::F64(score as f64),
            SortSource::Doc => SortValue::I64(doc as i64),
            SortSource::Column { desc, mode, .. } => self.columns[i]
                .as_ref()
                .map(|c| c.read(doc, *desc, mode.as_deref()))
                .unwrap_or(SortValue::Missing),
        }
    }

    fn prune(&mut self) {
        prune_by(&mut self.buf, self.limit, &self.desc);
        // once the buffer is full the worst kept entry becomes the bar to clear
        if self.buf.len() >= self.limit {
            let worst = self
                .buf
                .iter()
                .max_by(|a, b| cmp_sorted(&a.sort, &b.sort, &self.desc))
                .map(|c| c.sort[0].clone());
            self.cutoff = worst;
        }
    }
}

impl tantivy::collector::SegmentCollector for SortSegmentCollector {
    type Fruit = Vec<Cand>;

    /// Vectorised path: for a single numeric sort key the whole block of
    /// matching documents is pulled out of the columnar in one call.
    fn collect_block(&mut self, docs: &[tantivy::DocId]) {
        if self.block.is_none() {
            for &d in docs {
                self.collect(d, 0.0);
            }
            return;
        }
        let (col, ty) = {
            let sc = self.columns[0].as_ref().unwrap();
            let (_, num, ty) = &sc.per_segment[0];
            (num.clone().unwrap(), ty.unwrap())
        };
        let mut block = self.block.take().unwrap();
        block.fetch_block(docs, &col);
        let desc = self.desc[0];
        let after = self.after.as_ref().and_then(|a| a.first().cloned());
        for (doc, raw) in block.iter_docid_vals(docs, &col) {
            let Some(v) = decode_col_value(raw, ty) else { continue };
            // the vectorized path has to honour the page boundary too
            if let Some(marker) = &after {
                let ord = v.cmp_asc(marker);
                let ord = if desc { ord.reverse() } else { ord };
                if ord != Ordering::Greater {
                    continue;
                }
            }
            if let Some(cut) = &self.cutoff {
                let ord = v.cmp_asc(cut);
                let ord = if desc { ord.reverse() } else { ord };
                if ord == Ordering::Greater {
                    continue;
                }
            }
            self.buf.push(Cand {
                shard: 0,
                addr: DocAddress::new(self.segment_ord, doc),
                score: 1.0,
                sort: vec![v],
                seq: u64::MAX,
            });
            if self.limit > 0 && self.buf.len() >= self.limit.saturating_mul(4).max(512) {
                self.prune();
            }
        }
        self.block = Some(block);
    }

    fn collect(&mut self, doc: tantivy::DocId, score: tantivy::Score) {
        let first = self.read_key(0, doc, score);
        // cheap rejection before allocating anything for this document
        if let Some(cut) = &self.cutoff {
            let ord = first.cmp_asc(cut);
            let ord = if self.desc[0] { ord.reverse() } else { ord };
            if ord == Ordering::Greater {
                return;
            }
        }
        let mut sort = Vec::with_capacity(self.sources.len());
        sort.push(first);
        for i in 1..self.sources.len() {
            sort.push(self.read_key(i, doc, score));
        }
        // anything at or before where the last page ended is already served
        if let Some(after) = &self.after {
            let mut behind = true;
            for (i, want_desc) in self.desc.iter().enumerate() {
                let Some(marker) = after.get(i) else { break };
                let ord = sort[i].cmp_asc(marker);
                let ord = if *want_desc { ord.reverse() } else { ord };
                match ord {
                    Ordering::Greater => {
                        behind = false;
                        break;
                    }
                    Ordering::Less => {
                        behind = true;
                        break;
                    }
                    Ordering::Equal => {}
                }
            }
            if behind {
                return;
            }
        }
        self.buf.push(Cand {
            shard: 0,
            addr: DocAddress::new(self.segment_ord, doc),
            score,
            sort,
            seq: u64::MAX,
        });
        if self.limit > 0 && self.buf.len() >= self.limit.saturating_mul(4).max(512) {
            self.prune();
        }
    }

    fn harvest(mut self) -> Self::Fruit {
        prune_by(&mut self.buf, self.limit, &self.desc);
        self.buf
    }
}

/// A matched document before its `_source` is read. Reading stored fields is by
/// far the most expensive part of a hit, so it is deferred until the page is known.
struct Cand {
    shard: usize,
    addr: DocAddress,
    score: f32,
    sort: Vec<SortValue>,
    /// the order this document's write arrived in; filled once the candidates
    /// from every segment are together, and only used to settle ties
    seq: u64,
}

/// Is this sort key a date field, whose values need rescaling on the way out?
/// Does this sort key need rescaling on the way out?
///
/// The column counts nanoseconds either way, but a `date` reports
/// milliseconds and a `date_nanos` reports the nanoseconds themselves -- that
/// resolution is the whole reason for the second type.
fn date_sort_key(store: &Store, targets: &[String], field: &str) -> bool {
    targets
        .iter()
        .filter_map(|n| store.get(n))
        .any(|st| st.read().mapping.type_of(field) == Some("date"))
}

/// Read one `search_after` element back into the value the sort produced.
fn sort_value_from_json(v: &Value, is_date: bool) -> SortValue {
    match v {
        // the columns are read as f64, so the marker is put in the same shape
        // before it is compared with them
        Value::Number(n) => {
            let scale = if is_date { 1e6 } else { 1.0 };
            SortValue::F64(n.as_f64().unwrap_or(0.0) * scale)
        }
        // a marker for a date field may be written as a date rather than as
        // the number the column holds
        Value::String(s) if is_date => crate::store::canonical_date(v)
            .and_then(|d| crate::store::parse_date_lenient(&d))
            .map(|d| SortValue::F64(d.unix_timestamp_nanos() as f64))
            .unwrap_or_else(|| SortValue::Str(s.clone())),
        Value::String(s) => SortValue::Str(s.clone()),
        Value::Null => SortValue::Missing,
        other => SortValue::Str(other.to_string()),
    }
}

/// Read each candidate's arrival order out of the index.
///
/// The writer spreads one bulk request across its worker threads, so which
/// segment a document lands in -- and what doc id it gets there -- does not
/// follow the order it was sent in. `_seq` does, and reading it here rather
/// than while collecting keeps it off the path every matching document walks:
/// by now the field is only read for the handful of candidates that survived.
fn fill_seq(
    cands: &mut [Cand],
    searchers: &[(String, Searcher, std::sync::Arc<parking_lot::RwLock<IdxState>>)],
) {
    let mut cols: std::collections::HashMap<(usize, u32), Option<tantivy::columnar::Column<u64>>> =
        std::collections::HashMap::new();
    for c in cands.iter_mut() {
        let (shard, seg) = (c.shard, c.addr.segment_ord);
        let col = cols.entry((shard, seg)).or_insert_with(|| {
            let (_, searcher, _) = searchers.get(shard)?;
            let reader = searcher.segment_readers().get(seg as usize)?;
            reader.fast_fields().u64("_seq").ok()
        });
        if let Some(col) = col {
            c.seq = col.first(c.addr.doc_id).unwrap_or(u64::MAX);
        }
    }
}

fn cmp_cands(a: &Cand, b: &Cand, sort_keys: &[SortKey]) -> Ordering {
    // ties fall back to document order, which is insertion order within a
    // shard -- otherwise equally-scored hits come back in a different order
    // from one run to the next
    let by_doc = || {
        a.shard
            .cmp(&b.shard)
            .then(a.seq.cmp(&b.seq))
            .then(a.addr.segment_ord.cmp(&b.addr.segment_ord))
            .then(a.addr.doc_id.cmp(&b.addr.doc_id))
    };
    if sort_keys.is_empty() {
        return b
            .score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(by_doc);
    }
    for (i, k) in sort_keys.iter().enumerate() {
        let ord = a.sort[i].cmp_asc(&b.sort[i]);
        let ord = if k.desc && ord != Ordering::Equal { ord.reverse() } else { ord };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    by_doc()
}

/// Keep only the best `want` candidates, pruning in amortised linear time
/// rather than sorting every match.
fn prune(cands: &mut Vec<Cand>, want: usize, sort_keys: &[SortKey]) {
    if want == 0 || cands.len() <= want {
        return;
    }
    cands.select_nth_unstable_by(want - 1, |a, b| cmp_cands(a, b, sort_keys));
    cands.truncate(want);
}

/// Aggregations address fields by name; rewrite each onto the JSON view that
/// actually carries the doc values for it.
const NUMERIC_AGGS: &[&str] = &[
    "avg", "sum", "min", "max", "stats", "extended_stats", "percentiles", "histogram",
    "date_histogram",
];

/// Reject a numeric metric over a string field the way OpenSearch does.
fn check_agg_types(node: &Value, ctx: &Ctx) -> std::result::Result<(), Response> {
    check_agg_node(node, ctx, "")
}

/// Numeric parameter bounds OpenSearch enforces; `owner` is the aggregation
/// name the message has to quote.
fn check_agg_params(name: &str, def: &Value, owner: &str) -> std::result::Result<(), Response> {
    let bad = |param: &str, got: f64, bound: &str| {
        err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "[{param}] must be greater than {bound}. Found [{}] in [{owner}]",
                if got.fract() == 0.0 && param == "precisionThreshold" {
                    format!("{}", got as i64)
                } else {
                    format!("{got:?}")
                }
            ),
        )
    };
    let num = |k: &str| def.get(k).and_then(|v| v.as_f64());
    match name {
        "extended_stats" => {
            if let Some(v) = num("sigma") {
                if v < 0.0 {
                    return Err(bad("sigma", v, "or equal to 0"));
                }
            }
        }
        "cardinality" => {
            if let Some(v) = num("precision_threshold") {
                if v < 0.0 {
                    return Err(bad("precisionThreshold", v, "or equal to 0"));
                }
            }
        }
        "percentiles" | "median_absolute_deviation" => {
            if let Some(d) = def.pointer("/hdr/number_of_significant_value_digits") {
                if !matches!(d.as_i64(), Some(0..=5)) {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        "[numberOfSignificantValueDigits] must be between 0 and 5",
                    ));
                }
            }
            // `percents` names which percentiles to report, so an empty or
            // unreadable list leaves nothing to compute
            if let Some(p) = def.get("percents") {
                let ok = p
                    .as_array()
                    .map(|a| !a.is_empty() && a.iter().all(|v| v.as_f64().is_some()))
                    .unwrap_or(false);
                if !ok {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "x_content_parse_exception",
                        "[percents] must be a non-empty list of numbers",
                    ));
                }
            }
            if let Some(v) = num("compression") {
                if v <= 0.0 {
                    return Err(bad("compression", v, "0"));
                }
            }
            // the tdigest sketch takes its own compression, and admits 0
            if let Some(v) = def.pointer("/tdigest/compression").and_then(|v| v.as_f64()) {
                if v < 0.0 {
                    return Err(bad("compression", v, "or equal to 0"));
                }
            }
        }
        "moving_fn" | "moving_avg" => {
            // a window of zero or fewer has nothing to average over
            if let Some(v) = num("window") {
                if v < 1.0 {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        "[window] must be a positive, non-zero integer.",
                    ));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Walk the aggregation tree applying only the checks that need no mapping.
fn check_agg_bounds(node: &Value, owner: &str) -> std::result::Result<(), Response> {
    let Some(o) = node.as_object() else { return Ok(()) };
    for (name, def) in o {
        check_agg_params(name, def, owner)?;
        let next_owner = if owner.is_empty() { name.as_str() } else { owner };
        check_agg_bounds(def, next_owner)?;
    }
    Ok(())
}

fn check_agg_node(node: &Value, ctx: &Ctx, owner: &str) -> std::result::Result<(), Response> {
    let Some(o) = node.as_object() else { return Ok(()) };
    for (name, def) in o {
        check_agg_params(name, def, owner)?;
        // `terms` is also the name of a query, which appears inside filter
        // aggregations and inside multi_terms; only an object made entirely of
        // terms-aggregation options is one of those
        const TERMS_AGG_OPTIONS: &[&str] = &[
            "field", "script", "size", "shard_size", "order", "include", "exclude",
            "min_doc_count", "shard_min_doc_count", "missing", "execution_hint",
            "collect_mode", "value_type", "format", "show_term_doc_count_error",
        ];
        if name == "terms" && def.get("field").is_none() && def.get("script").is_none() {
            let all_options = def
                .as_object()
                .map(|o| !o.is_empty() && o.keys().all(|k| TERMS_AGG_OPTIONS.contains(&k.as_str())))
                .unwrap_or(false);
            if all_options {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    "Required one of fields [field, script], but none were specified. ",
                ));
            }
        }
        if name == "terms" {
            for pass in ["include", "exclude"] {
                if !matches!(def.get(pass), Some(Value::String(_))) {
                    continue;
                }
                let field = def.get("field").and_then(|f| f.as_str()).unwrap_or("");
                let base = field.strip_suffix(".keyword").unwrap_or(field);
                if !matches!(
                    ctx.mapping.type_of(base),
                    None | Some("keyword" | "text" | "wildcard")
                ) {
                    return Err(err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "illegal_argument_exception",
                        format!(
                            "Aggregation [{owner}] cannot support regular expression style \
                             include/exclude settings as they can only be applied to string \
                             fields. Use an array of values for include/exclude clauses"
                        ),
                    ));
                }
            }
        }
        if NUMERIC_AGGS.contains(&name.as_str()) {
            if let Some(f) = def.get("field").and_then(|v| v.as_str()) {
                // a field the mapping never named is still a text field if
                // text is all it has ever held
                let dynamic_text = ctx.mapping.type_of(f).is_none()
                    && ctx.kinds_complete
                    && ctx
                        .observed_kinds
                        .get(f)
                        .map(|k| *k == crate::store::KIND_STR)
                        .unwrap_or(false);
                if matches!(ctx.mapping.type_of(f), Some("text") | Some("keyword")) || dynamic_text
                {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        format!(
                            "Field [{f}] of type [{}] is not supported for aggregation [{name}]",
                            ctx.mapping.type_of(f).unwrap_or("text")
                        ),
                    ));
                }
            }
        }
        // at the top level the key is the user's name for the aggregation
        let next_owner = if owner.is_empty() { name.as_str() } else { owner };
        check_agg_node(def, ctx, next_owner)?;
    }
    Ok(())
}

/// Dates in aggregation parameters may be date-only; tantivy needs RFC3339.
fn normalize_agg_dates(node: &mut Value) {
    match node {
        Value::Object(o) => {
            for (k, v) in o.iter_mut() {
                if matches!(k.as_str(), "min" | "max" | "from" | "to") {
                    if let Value::String(s) = v {
                        if s.len() == 10 && s.matches('-').count() == 2 {
                            *v = json!(format!("{s}T00:00:00Z"));
                            continue;
                        }
                    }
                }
                normalize_agg_dates(v);
            }
        }
        Value::Array(a) => {
            for v in a {
                normalize_agg_dates(v);
            }
        }
        _ => {}
    }
}

/// Render a small subset of the query DSL as a tantivy query string, so a
/// `filter` aggregation nested inside another bucket can still run.
fn as_tantivy_query_string(q: &Value, ctx: &Ctx) -> Option<String> {
    let o = q.as_object()?;
    let (kind, body) = o.iter().next()?;
    match kind.as_str() {
        "match_all" => Some("*".to_string()),
        "term" | "match" | "match_phrase" => {
            let (field, spec) = body.as_object()?.iter().next()?;
            let value = spec.get("value").or_else(|| spec.get("query")).unwrap_or(spec);
            let col = ctx.column_name(field, kind != "term");
            let text = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            Some(format!("{col}:\"{text}\""))
        }
        "range" => {
            let (field, spec) = body.as_object()?.iter().next()?;
            let col = ctx.column_name(field, false);
            let lo = spec.get("gte").or_else(|| spec.get("gt"));
            let hi = spec.get("lte").or_else(|| spec.get("lt"));
            let fmt = |v: Option<&Value>| match v {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => "*".to_string(),
            };
            Some(format!("{col}:[{} TO {}]", fmt(lo), fmt(hi)))
        }
        "bool" => {
            let mut parts = Vec::new();
            for key in ["must", "filter"] {
                if let Some(list) = body.get(key) {
                    let items: Vec<Value> = match list {
                        Value::Array(a) => a.clone(),
                        other => vec![other.clone()],
                    };
                    for it in items {
                        parts.push(format!("+({})", as_tantivy_query_string(&it, ctx)?));
                    }
                }
            }
            if let Some(list) = body.get("must_not") {
                let items: Vec<Value> = match list {
                    Value::Array(a) => a.clone(),
                    other => vec![other.clone()],
                };
                for it in items {
                    parts.push(format!("-({})", as_tantivy_query_string(&it, ctx)?));
                }
            }
            if parts.is_empty() { None } else { Some(parts.join(" ")) }
        }
        _ => None,
    }
}

/// Nested `filter` aggregations become tantivy's own filter, which speaks
/// query strings. Top-level ones are handled by running a separate search.
fn lower_nested_filters(node: &mut Value, ctx: &Ctx) {
    let Some(o) = node.as_object_mut() else { return };
    for (_, def) in o.iter_mut() {
        if let Some(sub) = def.get_mut("aggs") {
            if let Some(subo) = sub.as_object_mut() {
                for (_, sdef) in subo.iter_mut() {
                    if let Some(f) = sdef.get("filter").cloned() {
                        if !f.is_string() {
                            match as_tantivy_query_string(&f, ctx) {
                                Some(qs) => {
                                    sdef.as_object_mut().unwrap().insert("filter".into(), json!(qs));
                                }
                                None => {}
                            }
                        }
                    }
                }
                lower_nested_filters(sub, ctx);
            }
        }
    }
}

/// tantivy cannot order a `terms` aggregation by a nested bucket's doc_count,
/// so strip that order and reapply it to the finished buckets ourselves.
/// Lucene's `StringHelper.murmurhash3_x86_32`, which is what OpenSearch hashes
/// a string term with when a terms aggregation is split into partitions.
fn murmur3_x86_32(data: &[u8], seed: u32) -> i32 {
    const C1: u32 = 0xcc9e_2d51;
    const C2: u32 = 0x1b87_3593;
    let mut h1 = seed;
    let blocks = data.len() / 4;
    for i in 0..blocks {
        let mut k1 = u32::from_le_bytes([data[i * 4], data[i * 4 + 1], data[i * 4 + 2], data[i * 4 + 3]]);
        k1 = k1.wrapping_mul(C1).rotate_left(15).wrapping_mul(C2);
        h1 ^= k1;
        h1 = h1.rotate_left(13).wrapping_mul(5).wrapping_add(0xe654_6b64);
    }
    let tail = &data[blocks * 4..];
    let mut k1: u32 = 0;
    if tail.len() >= 3 {
        k1 ^= (tail[2] as u32) << 16;
    }
    if tail.len() >= 2 {
        k1 ^= (tail[1] as u32) << 8;
    }
    if !tail.is_empty() {
        k1 ^= tail[0] as u32;
        k1 = k1.wrapping_mul(C1).rotate_left(15).wrapping_mul(C2);
        h1 ^= k1;
    }
    h1 ^= data.len() as u32;
    h1 ^= h1 >> 16;
    h1 = h1.wrapping_mul(0x85eb_ca6b);
    h1 ^= h1 >> 13;
    h1 = h1.wrapping_mul(0xc2b2_ae35);
    h1 ^= h1 >> 16;
    h1 as i32
}

/// HPPC's `BitMixer.mix64`, the numeric counterpart of the hash above.
fn mix64(v: i64) -> i64 {
    let mut z = v as u64;
    z = (z ^ (z >> 32)).wrapping_mul(0x4cd6_944c_5cc2_0b6d);
    z = (z ^ (z >> 29)).wrapping_mul(0xfc12_c5b1_9d32_59e9);
    (z ^ (z >> 32)) as i64
}

/// Which partition a terms bucket key falls in, hashed the way OpenSearch
/// hashes it so the same key lands in the same partition here.
fn term_partition(key: &Value, num: i64) -> i64 {
    let hash = match key {
        Value::String(s) => murmur3_x86_32(s.as_bytes(), 31) as i64,
        Value::Number(n) => mix64(n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64)),
        Value::Bool(b) => mix64(*b as i64),
        _ => 0,
    };
    hash.rem_euclid(num.max(1))
}

/// A terms aggregation may ask for one slice of the term space rather than the
/// whole of it. tantivy has no such notion, so the slice is taken here: the
/// request goes down without the `include`, asking for enough terms that the
/// wanted partition is whole, and the rest are dropped from the answer.
fn extract_partitions(node: &mut Value) -> Vec<(String, i64, i64, usize)> {
    let mut out = Vec::new();
    let Some(o) = node.as_object_mut() else { return out };
    for (name, def) in o.iter_mut() {
        let Some(terms) = def.get_mut("terms").and_then(|t| t.as_object_mut()) else {
            continue;
        };
        let part = terms.get("include").and_then(|i| i.get("partition")).and_then(|v| v.as_i64());
        let num = terms
            .get("include")
            .and_then(|i| i.get("num_partitions"))
            .and_then(|v| v.as_i64());
        let (Some(part), Some(num)) = (part, num) else { continue };
        let size = terms.get("size").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
        terms.remove("include");
        // ask for the whole term space, since which terms fall in the wanted
        // partition is not known until they are hashed
        terms.insert("size".into(), json!(65_536));
        out.push((name.clone(), part, num, size));
    }
    out
}

fn apply_partitions(result: &mut Value, parts: &[(String, i64, i64, usize)]) {
    for (name, part, num, size) in parts {
        let Some(buckets) = result.pointer_mut(&format!("/{name}/buckets")) else { continue };
        let Some(list) = buckets.as_array_mut() else { continue };
        list.retain(|b| b.get("key").map(|k| term_partition(k, *num) == *part).unwrap_or(false));
        list.truncate(*size);
    }
}

fn extract_bucket_orders(node: &mut Value) -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    let Some(o) = node.as_object_mut() else { return out };
    for (name, def) in o.iter_mut() {
        let Some(terms) = def.get_mut("terms") else { continue };
        let Some(order) = terms.get("order").cloned() else { continue };
        let Some(oo) = order.as_object() else { continue };
        let Some((key, dir)) = oo.iter().next() else { continue };
        if !key.contains('.') {
            continue;
        }
        let sub = key.split('.').next().unwrap_or("").to_string();
        let desc = dir.as_str().map(|d| d == "desc").unwrap_or(false);
        terms.as_object_mut().unwrap().remove("order");
        out.push((name.clone(), sub, desc));
    }
    out
}

fn apply_bucket_orders(result: &mut Value, orders: &[(String, String, bool)]) {
    for (agg, sub, desc) in orders {
        let Some(buckets) = result.pointer_mut(&format!("/{agg}/buckets")) else { continue };
        let Some(list) = buckets.as_array_mut() else { continue };
        list.sort_by(|a, b| {
            let av = a.pointer(&format!("/{sub}/doc_count")).and_then(|v| v.as_i64()).unwrap_or(0);
            let bv = b.pointer(&format!("/{sub}/doc_count")).and_then(|v| v.as_i64()).unwrap_or(0);
            if *desc { bv.cmp(&av) } else { av.cmp(&bv) }
        });
    }
}

/// A string `missing` on a field the index holds no values for.
///
/// The columns a text substitute would need do not exist, so the aggregation
/// reads nothing and answers zero. Every document takes the same substitute
/// though, which makes the distinct count one whatever that value is -- so a
/// numeric stand-in gives the right answer through a column that does exist.
/// Only applied where the field is known to hold nothing at all.
fn substitute_unusable_missing(body: &mut Value, ctx: &Ctx) {
    let Some(o) = body.as_object_mut() else { return };
    if !matches!(o.get("missing"), Some(Value::String(_))) {
        return;
    }
    let Some(field) = o.get("field").and_then(|f| f.as_str()) else { return };
    let unobserved = ctx.kinds_complete
        && ctx.observed_kinds.get(field).map(|k| *k == 0).unwrap_or(true);
    if unobserved {
        o.insert("missing".into(), json!(0));
    }
}

fn rewrite_agg_fields(node: &mut Value, ctx: &Ctx) {
    match node {
        Value::Object(o) => {
            if let Some(Value::String(f)) = o.get("field") {
                // `_raw` carries both the untokenised strings and the numerics,
                // so it is the right column for every agg except one over an
                // explicitly analysed text field.
                let base = f.strip_suffix(".keyword").unwrap_or(f);
                // Both views carry the numerics, but resolving a purely numeric
                // path is measurably cheaper on `_dyn` -- `_raw` also holds a
                // string column for every path, which the lookup has to consider.
                // Strings must stay on `_raw`, whose values are untokenised.
                let numeric_only = std::env::var("OBSEARCH_NO_NUMERIC_DYN_AGG").is_err()
                    && ctx
                    .observed_kinds
                    .get(base)
                    .map(|k| {
                        *k != 0 && k & (crate::store::KIND_STR | crate::store::KIND_DATE) == 0
                    })
                    .unwrap_or(false);
                let analyzed = matches!(ctx.view(f, false), View::Dyn)
                    && ctx.mapping.type_of(f).is_some();
                let prefix = if analyzed || numeric_only {
                    crate::store::DYN
                } else {
                    crate::store::RAW
                };
                let rewritten = format!("{prefix}.{base}");
                o.insert("field".into(), json!(rewritten));
            }
            for (k, v) in o.iter_mut() {
                if k == "cardinality" {
                    substitute_unusable_missing(v, ctx);
                }
                rewrite_agg_fields(v, ctx);
            }
        }
        Value::Array(a) => {
            for v in a {
                rewrite_agg_fields(v, ctx);
            }
        }
        _ => {}
    }
}

/// tantivy's aggregation model has no room for OpenSearch's `meta`, and it
/// spells the sub-aggregation key `aggs`. Strip one, normalise the other, and
/// remember the metadata so it can be put back on the response.
fn normalize_aggs(node: &mut Value, metas: &mut Vec<(String, Value)>, top: bool) {
    let Some(map) = node.as_object_mut() else { return };
    for (name, def) in map.iter_mut() {
        let Some(d) = def.as_object_mut() else { continue };
        if let Some(sub) = d.remove("aggregations") {
            d.insert("aggs".into(), sub);
        }
        if let Some(meta) = d.remove("meta") {
            if top {
                metas.push((name.clone(), meta));
            }
        }
        // `_term` and `_time` are the old spellings of `_key`, kept working
        // for the aggregations that were named before it was renamed
        for agg in d.values_mut() {
            let Some(order) = agg.get_mut("order").and_then(|o| o.as_object_mut()) else {
                continue;
            };
            for old in ["_term", "_time"] {
                if let Some(dir) = order.remove(old) {
                    order.insert("_key".into(), dir);
                }
            }
        }
        if let Some(sub) = d.get_mut("aggs") {
            normalize_aggs(sub, metas, false);
        }
    }
}

/// Recompute extended_stats moments the way OpenSearch does, from the raw
/// sums, so the last-bit float results agree.
fn recompute_extended_stats(v: &mut Value) {
    match v {
        Value::Object(o) => {
            let ready = o.contains_key("sum_of_squares")
                && o.contains_key("count")
                && o.contains_key("sum");
            if ready {
                let count = o.get("count").and_then(|x| x.as_f64()).unwrap_or(0.0);
                let sum = o.get("sum").and_then(|x| x.as_f64()).unwrap_or(0.0);
                let sq = o.get("sum_of_squares").and_then(|x| x.as_f64()).unwrap_or(0.0);
                if count > 0.0 {
                    let centred = sq - ((sum * sum) / count);
                    let var = centred / count;
                    let var_samp =
                        if count > 1.0 { centred / (count - 1.0) } else { f64::NAN };
                    let sd = var.sqrt();
                    let sd_samp = var_samp.sqrt();
                    let sigma = o
                        .get("std_deviation_bounds")
                        .and_then(|b| b.get("upper"))
                        .and_then(|x| x.as_f64())
                        .and_then(|upper| {
                            let mean = sum / count;
                            let old_sd =
                                o.get("std_deviation").and_then(|x| x.as_f64()).unwrap_or(0.0);
                            if old_sd != 0.0 { Some((upper - mean) / old_sd) } else { None }
                        })
                        .map(|raw| (raw * 1e6).round() / 1e6) // undo float noise in the derivation
                        .unwrap_or(2.0);
                    let mean = sum / count;
                    o.insert("variance".into(), json!(var));
                    o.insert("variance_population".into(), json!(var));
                    o.insert("variance_sampling".into(), json!(var_samp));
                    o.insert("std_deviation".into(), json!(sd));
                    o.insert("std_deviation_population".into(), json!(sd));
                    o.insert("std_deviation_sampling".into(), json!(sd_samp));
                    o.insert(
                        "std_deviation_bounds".into(),
                        json!({
                            "upper": mean + sd * sigma,
                            "lower": mean - sd * sigma,
                            "upper_population": mean + sd * sigma,
                            "lower_population": mean - sd * sigma,
                            "upper_sampling": mean + sd_samp * sigma,
                            "lower_sampling": mean - sd_samp * sigma,
                        }),
                    );
                }
            }
            for (_, child) in o.iter_mut() {
                recompute_extended_stats(child);
            }
        }
        Value::Array(a) => {
            for x in a {
                recompute_extended_stats(x);
            }
        }
        _ => {}
    }
}

fn reattach_meta(result: &mut Value, metas: &[(String, Value)]) {
    for (name, meta) in metas {
        if let Some(slot) = result.get_mut(name) {
            if let Some(o) = slot.as_object_mut() {
                o.insert("meta".into(), meta.clone());
            }
        }
    }
}

/// Replace `terms: {field: {index, id, path}}` with the terms held by that
/// document, the way OpenSearch resolves a terms lookup before searching.
/// Read a RoaringBitmap in its portable serialisation back into the integers
/// it holds.
///
/// A bitmap is how a caller sends a very long terms list compactly: the ids
/// are grouped by their high sixteen bits, and each group is written either as
/// a sorted array of the low bits or as a bitset over them.
/// The 64-bit form: a count of high words, then each high word followed by an
/// ordinary 32-bit bitmap of the low half.
fn decode_roaring64(bytes: &[u8]) -> Option<Vec<i64>> {
    let u32_at = |i: usize| -> Option<u32> {
        Some(u32::from_le_bytes([
            *bytes.get(i)?,
            *bytes.get(i + 1)?,
            *bytes.get(i + 2)?,
            *bytes.get(i + 3)?,
        ]))
    };
    let count = u32_at(0)? as usize;
    // the count is written as eight bytes, whose upper half is always zero
    if u32_at(4)? != 0 {
        return None;
    }
    let mut at = 8;
    let mut out = Vec::new();
    for _ in 0..count {
        let high = u32_at(at)? as i64;
        at += 4;
        let (low, used) = decode_roaring_at(bytes, at)?;
        at += used;
        out.extend(low.into_iter().map(|v| high << 32 | v));
    }
    Some(out)
}

fn decode_roaring(bytes: &[u8]) -> Option<Vec<i64>> {
    decode_roaring_at(bytes, 0).map(|(v, _)| v)
}

fn decode_roaring_at(bytes: &[u8], start: usize) -> Option<(Vec<i64>, usize)> {
    let bytes = bytes.get(start..)?;
    decode_roaring_inner(bytes)
}

fn decode_roaring_inner(bytes: &[u8]) -> Option<(Vec<i64>, usize)> {
    let u16_at = |i: usize| -> Option<u16> {
        Some(u16::from_le_bytes([*bytes.get(i)?, *bytes.get(i + 1)?]))
    };
    let u32_at = |i: usize| -> Option<u32> {
        Some(u32::from_le_bytes([
            *bytes.get(i)?,
            *bytes.get(i + 1)?,
            *bytes.get(i + 2)?,
            *bytes.get(i + 3)?,
        ]))
    };
    let cookie = u32_at(0)?;
    let mut at = 4;
    // the older cookie carries the container count separately; the newer one
    // packs it into the cookie and is followed by a bitset saying which
    // containers are run-encoded
    let (count, has_runs) = if cookie & 0xffff == 12_347 {
        (((cookie >> 16) + 1) as usize, true)
    } else if cookie == 12_346 {
        let n = u32_at(at)? as usize;
        at += 4;
        (n, false)
    } else {
        return None;
    };
    let mut runs = vec![false; count];
    if has_runs {
        let bytes_needed = count.div_ceil(8);
        for (i, run) in runs.iter_mut().enumerate() {
            *run = bytes.get(at + i / 8).map(|b| b >> (i % 8) & 1 == 1).unwrap_or(false);
        }
        at += bytes_needed;
    }
    let mut keys = Vec::with_capacity(count);
    for i in 0..count {
        keys.push((u16_at(at + i * 4)?, u16_at(at + i * 4 + 2)? as u32 + 1));
    }
    at += count * 4;
    // the offset header is only written when there are no runs, and the
    // containers follow it either way
    if !has_runs || count >= 4 {
        at += count * 4;
    }
    let mut out = Vec::new();
    for (i, (key, card)) in keys.iter().enumerate() {
        let high = (*key as i64) << 16;
        if runs[i] {
            let n = u16_at(at)? as usize;
            at += 2;
            for _ in 0..n {
                let start = u16_at(at)? as i64;
                let len = u16_at(at + 2)? as i64;
                at += 4;
                for v in start..=start + len {
                    out.push(high | v);
                }
            }
        } else if *card <= 4096 {
            for _ in 0..*card {
                out.push(high | u16_at(at)? as i64);
                at += 2;
            }
        } else {
            for word in 0..1024 {
                let mut bits = 0u64;
                for b in 0..8 {
                    bits |= (*bytes.get(at + word * 8 + b)? as u64) << (b * 8);
                }
                for bit in 0..64 {
                    if bits >> bit & 1 == 1 {
                        out.push(high | (word as i64 * 64 + bit));
                    }
                }
            }
            at += 8192;
        }
    }
    Some((out, at))
}

/// A `terms` clause may carry its list as a bitmap rather than as an array.
fn expand_bitmap_terms(node: &mut Value) {
    let Some(o) = node.as_object_mut() else { return };
    let is_bitmap = o
        .get("terms")
        .and_then(|t| t.get("value_type"))
        .and_then(|v| v.as_str())
        == Some("bitmap");
    if is_bitmap {
        if let Some(terms) = o.get_mut("terms").and_then(|t| t.as_object_mut()) {
            terms.remove("value_type");
            let fields: Vec<String> = terms.keys().cloned().collect();
            for f in fields {
                let encoded = match terms.get(&f) {
                    Some(Value::String(b)) => Some(b.clone()),
                    Some(Value::Array(a)) if a.len() == 1 => {
                        a[0].as_str().map(|s| s.to_string())
                    }
                    _ => None,
                };
                let Some(encoded) = encoded else { continue };
                // the 32-bit form starts with its cookie; the 64-bit form
                // starts with a count of the high words it groups by
                let Some(values) = base64_decode(&encoded).as_deref().and_then(|b| {
                    decode_roaring(b).or_else(|| decode_roaring64(b))
                }) else {
                    continue;
                };
                terms.insert(f, Value::Array(values.into_iter().map(|v| json!(v)).collect()));
            }
        }
    }
    for (_, v) in o.iter_mut() {
        match v {
            Value::Object(_) => expand_bitmap_terms(v),
            Value::Array(a) => a.iter_mut().for_each(expand_bitmap_terms),
            _ => {}
        }
    }
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut acc: u32 = 0;
    let mut bits = 0;
    let mut out = Vec::new();
    for c in text.bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = TABLE.iter().position(|t| *t == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Rewrite a `more_like_this` clause into the query it stands for.
///
/// The clause names documents rather than terms: the terms come from
/// analysing what those documents hold. `unlike` names documents whose terms
/// are to be taken back out of that set, which is what makes a query for
/// "like this one but not like that one" narrower rather than empty.
fn expand_more_like_this(store: &Store, targets: &[String], node: &mut Value) {
    let Some(o) = node.as_object_mut() else { return };
    for (_, v) in o.iter_mut() {
        match v {
            Value::Object(_) => expand_more_like_this(store, targets, v),
            Value::Array(a) => a.iter_mut().for_each(|x| expand_more_like_this(store, targets, x)),
            _ => {}
        }
    }
    let Some(spec) = o.get("more_like_this").cloned() else { return };

    let listed = |key: &str| -> Vec<Value> {
        match spec.get(key) {
            Some(Value::Array(a)) => a.clone(),
            Some(one) => vec![one.clone()],
            None => Vec::new(),
        }
    };
    let fields: Option<Vec<String>> = spec
        .get("fields")
        .and_then(|f| f.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect());

    // the documents an item names, or the one it carries
    let source_of_item = |item: &Value| -> Option<(Option<String>, Value)> {
        if let Some(doc) = item.get("doc") {
            return Some((None, doc.clone()));
        }
        let id = match item {
            Value::String(s) => s.clone(),
            other => other.get("_id").map(|v| match v {
                Value::String(s) => s.clone(),
                n => n.to_string(),
            })?,
        };
        let index = item.get("_index").and_then(|v| v.as_str()).map(|s| s.to_string());
        let names: Vec<String> = match index {
            Some(n) => vec![n],
            None => targets.to_vec(),
        };
        for n in names {
            let st = store.get(&n)?;
            let g = st.read();
            if let Some(src) = crate::api::read_source(&g, &id) {
                return Some((Some(id), src));
            }
        }
        None
    };

    // words a document contributes, by field
    let mut collect = |items: &[Value], out: &mut std::collections::BTreeMap<String, Vec<String>>,
                       ids: &mut Vec<String>| {
        for item in items {
            let Some((id, src)) = source_of_item(item) else { continue };
            if let Some(id) = id {
                ids.push(id);
            }
            let Some(obj) = src.as_object() else { continue };
            for (name, value) in obj {
                if fields.as_ref().map(|f| !f.iter().any(|w| w == name)).unwrap_or(false) {
                    continue;
                }
                let Some(text) = value.as_str() else { continue };
                for word in text.split_whitespace() {
                    out.entry(name.clone()).or_default().push(word.to_lowercase());
                }
            }
        }
    };

    let mut like: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    let mut unlike: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    let mut like_ids = Vec::new();
    let mut unlike_ids = Vec::new();
    collect(&listed("like"), &mut like, &mut like_ids);
    collect(&listed("unlike"), &mut unlike, &mut unlike_ids);

    let min_tf = spec.get("min_term_freq").and_then(|v| v.as_u64()).unwrap_or(2);
    let min_df = spec.get("min_doc_freq").and_then(|v| v.as_u64()).unwrap_or(5);

    let mut should = Vec::new();
    for (field, words) in &like {
        let taken_back: Vec<&String> = unlike.get(field).map(|w| w.iter().collect()).unwrap_or_default();
        let mut counts: std::collections::BTreeMap<&String, u64> = Default::default();
        for w in words {
            *counts.entry(w).or_insert(0) += 1;
        }
        for (word, tf) in counts {
            if tf < min_tf || taken_back.iter().any(|u| *u == word) {
                continue;
            }
            // how many documents hold the word, which is what min_doc_freq caps
            let df = targets
                .iter()
                .filter_map(|n| store.get(n))
                .map(|st| {
                    let g = st.read();
                    let ctx = Ctx {
                        fields: &g.fields,
                        mapping: &g.mapping,
                        index: &g.index,
                        max_terms_count: g.max_terms_count(),
                        observed_kinds: &g.observed_kinds,
                        kinds_complete: g.kinds_complete,
                        stats: &g.stats,
                    };
                    crate::query::build(&ctx, &json!({"match": {field.clone(): word}}))
                        .ok()
                        .and_then(|q| g.reader.searcher().search(&q, &Count).ok())
                        .unwrap_or(0) as u64
                })
                .sum::<u64>();
            if df < min_df {
                continue;
            }
            should.push(json!({"match": {field.clone(): word}}));
        }
    }

    let mut bool_q = serde_json::Map::new();
    if should.is_empty() {
        // nothing survived the thresholds, so nothing is like it
        *node = json!({"bool": {"must_not": [{"match_all": {}}]}});
        return;
    }
    bool_q.insert("should".into(), Value::Array(should));
    bool_q.insert("minimum_should_match".into(), json!(1));
    // the documents the query was built from are left out unless asked for
    let include = spec.get("include").and_then(|v| v.as_bool()).unwrap_or(false);
    if !include && !like_ids.is_empty() {
        bool_q.insert(
            "must_not".into(),
            json!([{"terms": {"_id": like_ids}}]),
        );
    }
    o.remove("more_like_this");
    *node = json!({"bool": Value::Object(bool_q)});
}

fn resolve_terms_lookups(store: &Store, node: &mut Value) -> std::result::Result<(), Response> {
    match node {
        Value::Object(o) => {
            if let Some(Value::Object(spec)) = o.get("terms").cloned().map(|v| v) {
                for (field, def) in spec {
                    let Some(d) = def.as_object() else { continue };
                    let (Some(index), Some(path)) = (
                        d.get("index").and_then(|v| v.as_str()),
                        d.get("path").and_then(|v| v.as_str()),
                    ) else {
                        continue;
                    };
                    let Some(st) = store.get(index) else {
                        return Err(no_such_index(index));
                    };
                    let pointer = format!("/{}", path.replace('.', "/"));
                    // the terms come from one named document, or from every
                    // document a query finds -- the second is how a caller
                    // says "whatever this group follows"
                    let list: Vec<Value> = if let Some(id) = d.get("id").and_then(|v| v.as_str()) {
                        let g = st.read();
                        let values = crate::api::read_source(&g, id)
                            .and_then(|src| src.pointer(&pointer).cloned())
                            .unwrap_or(Value::Array(vec![]));
                        match values {
                            Value::Array(a) => a,
                            other => vec![other],
                        }
                    } else if let Some(q) = d.get("query") {
                        let g = st.read();
                        let ctx = Ctx {
                            fields: &g.fields,
                            mapping: &g.mapping,
                            index: &g.index,
                            max_terms_count: g.max_terms_count(),
                            observed_kinds: &g.observed_kinds,
                            kinds_complete: g.kinds_complete,
                            stats: &g.stats,
                        };
                        let built = crate::query::build(&ctx, q).map_err(|e| {
                            err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string())
                        })?;
                        let searcher = g.reader.searcher();
                        let hits = searcher
                            .search(&built, &TopDocs::with_limit(g.max_terms_count()).order_by_score())
                            .map_err(|e| {
                                err(
                                    StatusCode::BAD_REQUEST,
                                    "search_phase_execution_exception",
                                    e.to_string(),
                                )
                            })?;
                        let mut out: Vec<Value> = Vec::new();
                        for (_, addr) in hits {
                            let Some((_, src)) = source_of(&searcher, &g, addr) else { continue };
                            // a document with nothing at that path contributes
                            // nothing, which is not the same as contributing a null
                            match src.pointer(&pointer) {
                                Some(Value::Array(a)) => {
                                    out.extend(a.iter().filter(|v| !v.is_null()).cloned())
                                }
                                Some(Value::Null) | None => {}
                                Some(one) => out.push(one.clone()),
                            }
                        }
                        out.sort_by_key(|v| v.to_string());
                        out.dedup();
                        out
                    } else {
                        continue;
                    };
                    // a lookup may point at a bitmap, whose value_type sits
                    // beside the field rather than inside it
                    let vt = o.get("terms").and_then(|t| t.get("value_type")).cloned();
                    let mut terms = json!({ field: list });
                    if let Some(vt) = vt {
                        terms["value_type"] = vt;
                    }
                    o.insert("terms".into(), terms);
                }
            }
            for (_, v) in o.iter_mut() {
                resolve_terms_lookups(store, v)?;
            }
            Ok(())
        }
        Value::Array(a) => {
            for v in a {
                resolve_terms_lookups(store, v)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn source_of(searcher: &Searcher, st: &IdxState, addr: DocAddress) -> Option<(String, Value)> {
    let doc: TantivyDocument = searcher.doc(addr).ok()?;
    let id = doc.get_first(st.fields.id)?.as_str()?.to_string();
    let raw = doc.get_first(st.fields.source)?.as_str()?.to_string();
    let src = serde_json::from_str(&raw).ok()?;
    Some((id, src))
}


/// Run an aggregation with the phase boundaries laid bare.
///
/// `searcher.search` folds the whole run into one call, so the phases are
/// driven here instead: a leaf collector per segment, the scan, the harvest,
/// and the merge. The numbers reported are the real elapsed time of each --
/// nothing is estimated -- though our engine has no separate initialise step
/// beyond building the collector, which is what `initialize` measures.
fn profiled_agg_search(
    searcher: &Searcher,
    q: &dyn tantivy::query::Query,
    aggs: Aggregations,
    ctxp: AggContextParams,
    ctx: &Ctx,
    request: Option<&Value>,
) -> (tantivy::Result<IntermediateAggregationResults>, Value) {
    use std::time::Instant;
    use tantivy::collector::{Collector, SegmentCollector};

    let mut ns = std::collections::BTreeMap::new();
    let mut collected = 0u64;
    let t = Instant::now();
    let collector = DistributedAggregationCollector::from_aggs(aggs.clone(), ctxp);
    ns.insert("initialize", t.elapsed().as_nanos() as u64);

    let started = Instant::now();
    let mut run = || -> tantivy::Result<IntermediateAggregationResults> {
        let weight = q.weight(tantivy::query::EnableScoring::disabled_from_searcher(searcher))?;
        let mut fruits = Vec::new();
        let (mut leaf_ns, mut collect_ns, mut post_ns) = (0u64, 0u64, 0u64);
        for (ord, reader) in searcher.segment_readers().iter().enumerate() {
            let t = Instant::now();
            let mut child = collector.for_segment(ord as u32, reader)?;
            leaf_ns += t.elapsed().as_nanos() as u64;

            let t = Instant::now();
            weight.for_each_no_score(reader, &mut |docs| {
                collected += docs.len() as u64;
                for d in docs {
                    child.collect(*d, 0.0);
                }
            })?;
            collect_ns += t.elapsed().as_nanos() as u64;

            let t = Instant::now();
            fruits.push(child.harvest());
            post_ns += t.elapsed().as_nanos() as u64;
        }
        ns.insert("build_leaf_collector", leaf_ns.max(1));
        ns.insert("collect", collect_ns.max(1));
        ns.insert("post_collection", post_ns.max(1));

        let t = Instant::now();
        let merged = collector.merge_fruits(fruits)?;
        ns.insert("build_aggregation", (t.elapsed().as_nanos() as u64).max(1));
        Ok(merged)
    };
    let res = run();
    for k in ["build_leaf_collector", "collect", "post_collection", "build_aggregation"] {
        ns.entry(k).or_insert(1);
    }
    let total: u64 = ns.values().sum();

    let breakdown: serde_json::Map<String, Value> = ns
        .iter()
        .map(|(k, v)| (k.to_string(), json!(v)))
        .chain(std::iter::once(("collect_count".to_string(), json!(collected))))
        .collect();
    let entries: Vec<Value> = aggs
        .iter()
        .map(|(name, agg)| {
            // the parsed model drops the knobs that only steer execution, so
            // the request itself is consulted for those
            let def = request
                .and_then(|r| r.get(name.as_str()))
                .cloned()
                .unwrap_or_else(|| serde_json::to_value(agg).unwrap_or(Value::Null));
            json!({
                "type": agg_profile_type(&def),
                "description": name,
                "time_in_nanos": total,
                "breakdown": breakdown,
                "debug": agg_profile_debug(&def, ctx),
            })
        })
        .collect();
    let profile = json!({
        "id": "[obsearch][0]",
        "searches": [],
        "aggregations": entries,
        "took": started.elapsed().as_nanos() as u64,
    });
    (res, profile)
}

/// The aggregator name OpenSearch reports for a request of this shape.
fn agg_profile_type(def: &Value) -> String {
    let kind = def.as_object().and_then(|o| o.keys().next().cloned()).unwrap_or_default();
    match kind.as_str() {
        "cardinality" => "CardinalityAggregator".into(),
        "terms" => {
            let body = def.get("terms").cloned().unwrap_or(Value::Null);
            let field = body.get("field").and_then(|f| f.as_str()).unwrap_or("");
            // the aggregator OpenSearch names depends on where the terms live:
            // an ordinal map for a keyword column, a hash map when the request
            // asks for one, and neither for a numeric column
            if field.starts_with(crate::store::DYN) {
                "NumericTermsAggregator".into()
            } else if body.get("execution_hint").and_then(|h| h.as_str()) == Some("map") {
                "MapStringTermsAggregator".into()
            } else {
                "GlobalOrdinalsStringTermsAggregator".into()
            }
        }
        "date_histogram" => "DateHistogramAggregator".into(),
        "histogram" => "NumericHistogramAggregator".into(),
        other => format!("{}Aggregator", capitalise_words(other)),
    }
}

fn capitalise_words(s: &str) -> String {
    s.split('_')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// Which collection strategy the run took.
///
/// OpenSearch names these after Lucene's collectors; the counts here describe
/// the equivalent choice our engine made -- a numeric column, or the hybrid
/// path a string field needs.
fn agg_profile_debug(def: &Value, ctx: &Ctx) -> Value {
    let Some((kind, body)) = def.as_object().and_then(|o| o.iter().next()) else {
        return json!({});
    };
    if kind == "terms" {
        // which kind of term was bucketed, which is what the strategy names
        let field = body.get("field").and_then(|f| f.as_str()).unwrap_or("");
        let strategy = if field.starts_with(crate::store::DYN) {
            "long_terms"
        } else {
            "string_terms"
        };
        return json!({"result_strategy": strategy});
    }
    if kind != "cardinality" {
        return json!({});
    }
    // the request has already been rewritten onto the internal JSON views
    let field = body.get("field").and_then(|f| f.as_str()).unwrap_or("");
    let field = field
        .strip_prefix("_raw.")
        .or_else(|| field.strip_prefix("_dyn."))
        .unwrap_or(field);
    let numeric = matches!(
        ctx.mapping.type_of(field),
        Some(
            "byte" | "short" | "integer" | "long" | "unsigned_long" | "float" | "half_float"
                | "double" | "scaled_float" | "date"
        )
    );
    json!({
        "empty_collectors_used": 0,
        "numeric_collectors_used": if numeric { 1 } else { 0 },
        "ordinals_collectors_used": 0,
        "ordinals_collectors_overhead_too_high": 0,
        "string_hashing_collectors_used": 0,
        "hybrid_collectors_used": if numeric { 0 } else { 1 },
    })
}


/// Weight aggregation buckets by `_doc_count`.
///
/// A document may stand for several, which every bucket count has to reflect.
/// Rather than a second collection pass, each bucket agg gains two helpers --
/// the sum of the field and how many documents carry it -- and the correction
/// is `doc_count + sum - carried`: documents without the field still count
/// once, documents with it count what it says.
const DC_SUM: &str = "__obs_dc_sum";
const DC_CNT: &str = "__obs_dc_count";

fn inject_doc_count_helpers(node: &mut Value) {
    let Some(o) = node.as_object_mut() else { return };
    for (_, def) in o.iter_mut() {
        let Some(d) = def.as_object_mut() else { continue };
        let is_bucket = d.keys().any(|k| {
            matches!(k.as_str(), "terms" | "histogram" | "date_histogram" | "range" | "filters")
        });
        let slot = if d.contains_key("aggregations") { "aggregations" } else { "aggs" };
        if let Some(sub) = d.get_mut(slot) {
            inject_doc_count_helpers(sub);
        }
        if !is_bucket {
            continue;
        }
        let subs = d.entry(slot).or_insert_with(|| json!({}));
        if let Some(m) = subs.as_object_mut() {
            m.insert(DC_SUM.into(), json!({"sum": {"field": "_doc_count"}}));
            m.insert(DC_CNT.into(), json!({"value_count": {"field": "_doc_count"}}));
        }
    }
}

fn apply_doc_counts(node: &mut Value) {
    match node {
        Value::Object(o) => {
            if let Some(Value::Array(buckets)) = o.get_mut("buckets") {
                for b in buckets.iter_mut() {
                    let sum = b.pointer(&format!("/{DC_SUM}/value")).and_then(|v| v.as_f64());
                    let cnt = b.pointer(&format!("/{DC_CNT}/value")).and_then(|v| v.as_f64());
                    if let (Some(sum), Some(cnt)) = (sum, cnt) {
                        let base = b.get("doc_count").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        b["doc_count"] = json!((base + sum - cnt).max(0.0) as u64);
                    }
                    if let Some(m) = b.as_object_mut() {
                        m.remove(DC_SUM);
                        m.remove(DC_CNT);
                    }
                }
                // the correction can reorder buckets a count-ordered agg sorted
                // before it was applied
                if let Some(Value::Array(buckets)) = o.get_mut("buckets") {
                    buckets.sort_by(|a, b| {
                        let get = |v: &Value| v.get("doc_count").and_then(|x| x.as_u64()).unwrap_or(0);
                        get(b).cmp(&get(a))
                    });
                }
            }
            for (_, v) in o.iter_mut() {
                apply_doc_counts(v);
            }
        }
        Value::Array(a) => {
            for v in a {
                apply_doc_counts(v);
            }
        }
        _ => {}
    }
}


/// An aggregation collector that may not be there.
///
/// Hits and aggregations were two separate searches over the same query, which
/// meant building the weight and walking every segment twice per index. At a
/// couple of hundred indices that second pass is most of the cost, so the two
/// now ride in one collector tuple -- and a request without aggregations still
/// needs something to occupy that slot.
struct MaybeAgg(Option<DistributedAggregationCollector>);

struct MaybeAggSegment(Option<tantivy::aggregation::AggregationSegmentCollector>);

impl tantivy::collector::Collector for MaybeAgg {
    type Fruit = Option<IntermediateAggregationResults>;
    type Child = MaybeAggSegment;

    fn for_segment(
        &self,
        ord: tantivy::SegmentOrdinal,
        reader: &tantivy::SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        Ok(MaybeAggSegment(match &self.0 {
            Some(c) => Some(c.for_segment(ord, reader)?),
            None => None,
        }))
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(
        &self,
        segment_fruits: Vec<Option<tantivy::Result<IntermediateAggregationResults>>>,
    ) -> tantivy::Result<Self::Fruit> {
        let Some(inner) = &self.0 else { return Ok(None) };
        let present: Vec<tantivy::Result<IntermediateAggregationResults>> =
            segment_fruits.into_iter().flatten().collect();
        if present.is_empty() {
            return Ok(None);
        }
        inner.merge_fruits(present).map(Some)
    }
}

impl tantivy::collector::SegmentCollector for MaybeAggSegment {
    type Fruit = Option<tantivy::Result<IntermediateAggregationResults>>;

    fn collect(&mut self, doc: tantivy::DocId, score: tantivy::Score) {
        if let Some(c) = &mut self.0 {
            c.collect(doc, score);
        }
    }

    /// Forwarding this matters: tantivy's aggregation collects a block at a
    /// time, and the default implementation would unroll it back into one call
    /// per document.
    fn collect_block(&mut self, docs: &[tantivy::DocId]) {
        if let Some(c) = &mut self.0 {
            c.collect_block(docs);
        }
    }

    fn harvest(self) -> Self::Fruit {
        self.0.map(|c| c.harvest())
    }
}



/// Whether the total can be had without walking the matches.
///
/// `Weight::count` reads the figure from the postings header for a term query
/// and from the segment for a match-all, and otherwise counts by iterating.
/// Splitting top-k from the count only pays where that shortcut exists: where
/// it does not, the count walks everything the pruned pass just avoided, and
/// two passes beat one only in the wrong direction.
fn count_without_walking(query_json: &Option<Value>) -> bool {
    let Some(q) = query_json else { return true };
    let Some(obj) = q.as_object() else { return false };
    if obj.len() != 1 {
        return false;
    }
    match obj.keys().next().map(|k| k.as_str()) {
        Some("match_all") => true,
        // a term query on one field, with no per-term options that would make
        // it something else
        Some("term") => obj
            .values()
            .next()
            .and_then(|v| v.as_object())
            .map(|o| {
                o.len() == 1
                    && o.values().next().map(|v| !v.is_object()).unwrap_or(false)
            })
            .unwrap_or(false),
        _ => false,
    }
}

/// How many documents the query matches.
///
/// `Weight::count` reads it straight from the postings header where the query
/// allows -- a term query with no deletions knows its own document frequency --
/// and falls back to walking the matches where it does not.
fn count_matches(
    searcher: &Searcher,
    query: &dyn tantivy::query::Query,
) -> tantivy::Result<usize> {
    let weight = query.weight(tantivy::query::EnableScoring::disabled_from_searcher(searcher))?;
    let mut total = 0usize;
    for reader in searcher.segment_readers() {
        total += weight.count(reader)? as usize;
    }
    Ok(total)
}

/// Run a shard's search, choosing where the per-segment work goes.
///
/// tantivy's own `search` hands the segments to the index's shared executor.
/// When a query fans out over many indices the outer parallelism already keeps
/// every core busy, and asking that same pool for per-segment parallelism from
/// inside it means each shard queues behind the others: measured on two hundred
/// empty indices, a search that should be free took 147us of elapsed time
/// waiting. One index at a time still wants the pool -- that is where
/// per-segment parallelism pays.
fn search_shard<C: tantivy::collector::Collector>(
    searcher: &Searcher,
    query: &dyn tantivy::query::Query,
    collector: &C,
    fanned_out: bool,
) -> tantivy::Result<C::Fruit> {
    if !fanned_out {
        return searcher.search(query, collector);
    }
    let scoring = if collector.requires_scoring() {
        tantivy::query::EnableScoring::enabled_from_statistics_provider(searcher, searcher)
    } else {
        tantivy::query::EnableScoring::disabled_from_searcher(searcher)
    };
    searcher.search_with_executor(
        query,
        collector,
        &tantivy::Executor::single_thread(),
        scoring,
    )
}


/// Wrap the terms a query looked for, wherever they appear in a hit's text.
///
/// The engine has no positional highlighter; what it does instead is analyse
/// the query's text the same way the field was analysed, then mark the tokens
/// of the stored value that match. That reproduces what a reader wants from a
/// highlight -- which words were found -- without a second index of offsets.
fn build_highlight(
    spec: &Value,
    source: &Value,
    query: &Option<Value>,
    mapping: &crate::store::Mapping,
    index: &tantivy::Index,
) -> Option<Value> {
    let fields = spec.get("fields")?;
    let patterns: Vec<(String, Value)> = match fields {
        Value::Object(o) => o.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        Value::Array(a) => a
            .iter()
            .filter_map(|f| f.as_object())
            .flat_map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>())
            .collect(),
        _ => return None,
    };
    let tag = |key: &str, fallback: &str| -> String {
        spec.get(key)
            .and_then(|v| match v {
                Value::Array(a) => a.first().and_then(|x| x.as_str()).map(|s| s.to_string()),
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| fallback.to_string())
    };
    let pre = tag("pre_tags", "<em>");
    let post = tag("post_tags", "</em>");
    let require_match = spec
        .get("require_field_match")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let asked = query_terms_by_field(query.as_ref());
    // every path the mapping knows, plus whatever the document itself carries
    let mut candidates: Vec<String> = mapping.types.keys().cloned().collect();
    if let Some(o) = source.as_object() {
        for k in o.keys() {
            if !candidates.contains(k) {
                candidates.push(k.clone());
            }
        }
    }
    candidates.sort();
    candidates.dedup();

    let mut out = serde_json::Map::new();
    for name in candidates {
        let wanted = patterns.iter().any(|(pat, _)| {
            pat == &name || pat == "*" || crate::store::glob_match(pat, &name)
        });
        if !wanted {
            continue;
        }
        // the value lives at the field's own path, or at its parent's when the
        // field is a multi-field of another
        let text = source
            .pointer(&format!("/{}", name.replace('.', "/")))
            .and_then(|v| v.as_str())
            .or_else(|| {
                let (parent, _) = name.rsplit_once('.')?;
                source
                    .pointer(&format!("/{}", parent.replace('.', "/")))?
                    .as_str()
            });
        let Some(text) = text else { continue };

        // a value longer than `ignore_above` was never indexed, so there is
        // nothing in it that could have matched
        if let Some(limit) = mapping
            .field_option(&name, "ignore_above")
            .and_then(|v| v.as_u64())
        {
            if text.chars().count() as u64 > limit {
                continue;
            }
        }
        // A per-field cap says how much of the value to analyse. The plain
        // highlighter returns what it analysed and nothing more; the unified
        // one still returns the whole field, having looked only that far for
        // something to mark.
        let opts = patterns
            .iter()
            .find(|(pat, _)| pat == &name || crate::store::glob_match(pat, &name))
            .map(|(_, o)| o.clone())
            .unwrap_or(Value::Null);
        let plain = opts.get("type").and_then(|t| t.as_str()) == Some("plain")
            || spec.get("type").and_then(|t| t.as_str()) == Some("plain");
        let text = match opts.get("max_analyzer_offset").and_then(|v| v.as_u64()) {
            Some(cap) if plain => {
                let cap = (cap as usize).min(text.len());
                &text[..cap]
            }
            _ => text,
        };
        let terms = terms_for_field(&asked, &name, require_match);
        if terms.is_empty() {
            continue;
        }
        let analyzer = mapping
            .field_option(&name, "analyzer")
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let marked = mark_terms(index, text, &terms, analyzer.as_deref(), &pre, &post);
        if let Some(marked) = marked {
            out.insert(name, json!([marked]));
        }
    }
    (!out.is_empty()).then(|| Value::Object(out))
}

/// The text each field was searched for, gathered from the query.
fn query_terms_by_field(query: Option<&Value>) -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    fn walk(node: &Value, out: &mut Vec<(String, String, bool)>) {
        let Some(o) = node.as_object() else {
            if let Value::Array(a) = node {
                a.iter().for_each(|v| walk(v, out));
            }
            return;
        };
        for (kind, body) in o {
            match kind.as_str() {
                "match" | "match_phrase" | "match_phrase_prefix" | "term" | "prefix"
                | "wildcard" | "match_bool_prefix" => {
                    if let Some(inner) = body.as_object() {
                        for (field, spec) in inner {
                            let text = match spec {
                                Value::String(s) => Some(s.clone()),
                                Value::Object(so) => so
                                    .get("value")
                                    .or_else(|| so.get("query"))
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string()),
                                other => other.as_f64().map(|n| n.to_string()),
                            };
                            if let Some(t) = text {
                                // a prefix or wildcard names the start of a
                                // word rather than the whole of it
                                let partial = matches!(
                                    kind.as_str(),
                                    "prefix" | "wildcard" | "match_phrase_prefix"
                                        | "match_bool_prefix"
                                );
                                out.push((field.clone(), t, partial));
                            }
                        }
                    }
                }
                "multi_match" => {
                    let text = body.get("query").and_then(|v| v.as_str()).unwrap_or("");
                    let fields = body.get("fields").and_then(|f| f.as_array());
                    match fields {
                        Some(fs) => {
                            for f in fs.iter().filter_map(|f| f.as_str()) {
                                out.push((
                                    f.split('^').next().unwrap_or(f).to_string(),
                                    text.to_string(),
                                    false,
                                ));
                            }
                        }
                        None => out.push(("*".to_string(), text.to_string(), false)),
                    }
                }
                "query_string" | "simple_query_string" => {
                    let text = body.get("query").and_then(|v| v.as_str()).unwrap_or("");
                    let field = body
                        .get("default_field")
                        .and_then(|v| v.as_str())
                        .unwrap_or("*");
                    out.push((field.to_string(), text.to_string(), false));
                }
                _ => walk(body, out),
            }
        }
    }
    if let Some(q) = query {
        walk(q, &mut out);
    }
    out
}

/// Which of the query's texts apply to this field.
fn terms_for_field(
    asked: &[(String, String, bool)],
    field: &str,
    require_match: bool,
) -> Vec<(String, bool)> {
    asked
        .iter()
        .filter(|(pat, _, _)| {
            if !require_match {
                return true;
            }
            pat == field
                || pat == "*"
                || crate::store::glob_match(pat, field)
                // `text*` names `text` and its multi-fields alike
                || field.starts_with(&format!("{pat}."))
        })
        .map(|(_, text, partial)| (text.clone(), *partial))
        .collect()
}

/// Mark the tokens of `text` that the query's words match.
fn mark_terms(
    index: &tantivy::Index,
    text: &str,
    queries: &[(String, bool)],
    analyzer: Option<&str>,
    pre: &str,
    post: &str,
) -> Option<String> {
    let mut whole: std::collections::HashSet<String> = Default::default();
    let mut starts: Vec<String> = Vec::new();
    for (q, partial) in queries {
        for tok in crate::query::analyze_text(index, q, analyzer) {
            if *partial {
                starts.push(tok);
            } else {
                whole.insert(tok);
            }
        }
    }
    if whole.is_empty() && starts.is_empty() {
        return None;
    }
    // walk the words of the original text, so punctuation and spacing survive
    let mut out = String::with_capacity(text.len() + 16);
    let mut marked = false;
    let mut rest = text;
    while !rest.is_empty() {
        let start = match rest.find(|c: char| c.is_alphanumeric()) {
            Some(i) => i,
            None => break,
        };
        out.push_str(&rest[..start]);
        let word = &rest[start..];
        let end = word
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(word.len());
        let (word, tail) = word.split_at(end);
        let lower = word.to_lowercase();
        if whole.contains(&lower) || starts.iter().any(|p| lower.starts_with(p)) {
            out.push_str(pre);
            out.push_str(word);
            out.push_str(post);
            marked = true;
        } else {
            out.push_str(word);
        }
        rest = tail;
    }
    out.push_str(rest);
    marked.then_some(out)
}


/// Build the `suggest` section of a response.
///
/// Two shapes are answered. A completion suggester looks for stored values
/// that begin with what has been typed so far; a term suggester takes text
/// that may be misspelled and offers, for each word, the closest word the
/// index actually holds.
fn build_suggest(
    store: &Store,
    targets: &[String],
    spec: &Value,
    typed_keys: bool,
) -> std::result::Result<Value, Response> {
    let Some(named) = spec.as_object() else { return Ok(json!({})) };
    let global_text = named.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let mut out = serde_json::Map::new();

    for (name, body) in named {
        if name == "text" {
            continue;
        }
        let Some(b) = body.as_object() else { continue };
        let text = b.get("text").and_then(|t| t.as_str()).unwrap_or(global_text);

        if let Some(c) = b.get("completion") {
            let entries = completion_suggest(store, targets, text, c)?;
            let key = if typed_keys { format!("completion#{name}") } else { name.clone() };
            out.insert(key, entries);
        } else if let Some(t) = b.get("term") {
            let entries = term_suggest(store, targets, text, t)?;
            let key = if typed_keys { format!("term#{name}") } else { name.clone() };
            out.insert(key, entries);
        } else if let Some(p) = b.get("phrase") {
            // a phrase suggester is answered per whole input rather than per
            // word; the options come from the same place the terms do
            let entries = term_suggest(store, targets, text, p)?;
            let key = if typed_keys { format!("phrase#{name}") } else { name.clone() };
            out.insert(key, entries);
        }
    }
    Ok(Value::Object(out))
}

/// Values that begin with what has been typed.
fn completion_suggest(
    store: &Store,
    targets: &[String],
    text: &str,
    spec: &Value,
) -> std::result::Result<Value, Response> {
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    let size = spec.get("size").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
    let skip_duplicates = spec
        .get("skip_duplicates")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let prefix = text.to_lowercase();

    let mut options: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = Default::default();
    for name in targets {
        let Some(st) = store.get(name) else { continue };
        let g = st.read();
        let searcher = g.reader.searcher();
        let all = tantivy::query::AllQuery;
        let addrs = searcher
            .search(&all, &tantivy::collector::DocSetCollector)
            .map_err(|e| {
                err(StatusCode::BAD_REQUEST, "search_phase_execution_exception", e.to_string())
            })?;
        for addr in addrs {
            let Some((id, source)) = source_of(&searcher, &g, addr) else { continue };
            // a completion field may hold one value or several
            // a completion value may be plain text, a list of them, or an
            // object carrying the inputs and the weight to rank them by
            let raw = source.pointer(&format!("/{}", field.replace('.', "/")));
            let texts = |v: &Value| -> Vec<String> {
                match v {
                    Value::String(s) => vec![s.clone()],
                    Value::Array(a) => {
                        a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()
                    }
                    _ => Vec::new(),
                }
            };
            // ... and a list may hold several such objects, each with its own
            // weight
            let mut weighted: Vec<(String, f64)> = Vec::new();
            match raw {
                Some(Value::Object(o)) => {
                    let w = o.get("weight").and_then(|x| x.as_f64()).unwrap_or(1.0);
                    weighted.extend(
                        o.get("input").map(&texts).unwrap_or_default().into_iter().map(|t| (t, w)),
                    );
                }
                Some(Value::Array(items)) if items.iter().any(|i| i.is_object()) => {
                    for item in items {
                        let Some(o) = item.as_object() else {
                            weighted.extend(texts(item).into_iter().map(|t| (t, 1.0)));
                            continue;
                        };
                        let w = o.get("weight").and_then(|x| x.as_f64()).unwrap_or(1.0);
                        weighted.extend(
                            o.get("input")
                                .map(&texts)
                                .unwrap_or_default()
                                .into_iter()
                                .map(|t| (t, w)),
                        );
                    }
                }
                Some(other) => weighted.extend(texts(other).into_iter().map(|t| (t, 1.0))),
                None => {}
            }
            for (v, weight) in weighted {
                if !v.to_lowercase().starts_with(&prefix) {
                    continue;
                }
                if skip_duplicates && !seen.insert(v.clone()) {
                    continue;
                }
                options.push(json!({
                    "text": v,
                    "_index": g.name,
                    "_id": id,
                    "_score": weight,
                    "_source": source,
                }));
            }
        }
    }
    // the heavier suggestion comes first; the text settles equal weights
    options.sort_by(|a, b| {
        let w = |v: &Value| v.get("_score").and_then(|s| s.as_f64()).unwrap_or(1.0);
        let t = |v: &Value| v.get("text").and_then(|s| s.as_str()).unwrap_or("").to_string();
        w(b).partial_cmp(&w(a)).unwrap_or(Ordering::Equal).then_with(|| t(a).cmp(&t(b)))
    });
    options.truncate(size);
    Ok(json!([{
        "text": text,
        "offset": 0,
        "length": text.chars().count(),
        "options": options,
    }]))
}

/// For each word, the closest word the index holds.
fn term_suggest(
    store: &Store,
    targets: &[String],
    text: &str,
    spec: &Value,
) -> std::result::Result<Value, Response> {
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    let size = spec.get("size").and_then(|v| v.as_u64()).unwrap_or(5) as usize;

    // every word the field actually holds, gathered once
    let mut vocabulary: std::collections::HashSet<String> = Default::default();
    for name in targets {
        let Some(st) = store.get(name) else { continue };
        let g = st.read();
        let searcher = g.reader.searcher();
        let all = tantivy::query::AllQuery;
        let Ok(addrs) = searcher.search(&all, &tantivy::collector::DocSetCollector) else {
            continue;
        };
        for addr in addrs {
            let Some((_, source)) = source_of(&searcher, &g, addr) else { continue };
            let Some(v) = source.pointer(&format!("/{}", field.replace('.', "/"))) else {
                continue;
            };
            let texts: Vec<String> = match v {
                Value::String(s) => vec![s.clone()],
                Value::Array(a) => {
                    a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()
                }
                _ => Vec::new(),
            };
            for t in texts {
                for word in t.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
                    vocabulary.insert(word.to_lowercase());
                }
            }
        }
    }

    let mut entries = Vec::new();
    let mut offset = 0usize;
    for word in text.split_whitespace() {
        let start = text[offset..].find(word).map(|i| offset + i).unwrap_or(offset);
        offset = start + word.len();
        let lower = word.to_lowercase();
        // a word the index already has needs no correction
        let mut options: Vec<(usize, String)> = if vocabulary.contains(&lower) {
            Vec::new()
        } else {
            vocabulary
                .iter()
                .filter_map(|cand| {
                    let d = edit_distance(&lower, cand);
                    (d > 0 && d <= 2).then(|| (d, cand.clone()))
                })
                .collect()
        };
        options.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        options.truncate(size);
        entries.push(json!({
            "text": word,
            "offset": start,
            "length": word.chars().count(),
            "options": options
                .into_iter()
                .map(|(d, t)| json!({
                    "text": t,
                    "score": 1.0 - (d as f64) / 10.0,
                    "freq": 1,
                }))
                .collect::<Vec<_>>(),
        }));
    }
    Ok(Value::Array(entries))
}

/// How many single-character edits turn one word into the other.
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

pub struct Outcome {
    pub took_ms: u64,
    pub skipped: u64,
    pub shards: u64,
    pub total: u64,
    pub hits: Vec<Value>,
    pub max_score: Option<f32>,
    pub aggs: Option<Value>,
    pub profile: Option<Value>,
    pub suggest: Option<Value>,
}

fn body_or_param<'a>(body: &'a Value, p: &'a Params, key: &str) -> Option<Value> {
    body.get(key).cloned().or_else(|| p.get(key).map(|v| json!(v)))
}

/// Request-level parameter validation, shared by search and msearch.
pub fn validate_params(body: &Value, p: &Params) -> std::result::Result<(), Response> {
    // an int total is only meaningful when the count is exact
    if p.get("rest_total_hits_as_int").map(|v| v == "true").unwrap_or(false) {
        let track = body.get("track_total_hits").cloned().or_else(|| {
            p.get("track_total_hits").map(|v| json!(v))
        });
        let inaccurate = match &track {
            Some(Value::Bool(false)) => None,
            Some(Value::Number(n)) => Some(n.to_string()),
            Some(Value::String(s)) if s == "false" => None,
            Some(Value::String(s)) if s != "true" => Some(s.clone()),
            _ => None,
        };
        if let Some(got) = inaccurate {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!(
                    "[rest_total_hits_as_int] cannot be used if the tracking of total hits is not accurate, got {got}"
                ),
            ));
        }
    }
    Ok(())
}

fn as_i64(v: Option<Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn as_usize(v: Option<Value>) -> Option<usize> {
    match v? {
        Value::Number(n) => n.as_u64().map(|x| x as usize),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Run a search across every resolved index and merge the results.
pub fn run(
    store: &Store,
    expr: &str,
    body: &Value,
    p: &Params,
) -> std::result::Result<Outcome, Response> {
    const BODY_KEYS: &[&str] = &[
        "query", "from", "size", "sort", "_source", "aggs", "aggregations", "post_filter",
        "highlight", "track_total_hits", "track_scores", "stored_fields", "docvalue_fields",
        "script_fields", "explain", "version", "seq_no_primary_term", "min_score", "timeout",
        "terminate_after", "search_after", "collapse", "rescore", "indices_boost", "profile",
        "suggest", "fields", "runtime_mappings", "slice", "pit", "stats", "batched_reduce_size",
        "ext", "knn",
    ];
    if let Some(o) = body.as_object() {
        for k in o.keys() {
            if !BODY_KEYS.contains(&k.as_str()) {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "parsing_exception",
                    format!("Unknown key for a START_OBJECT in [{k}]."),
                ));
            }
        }
    }
    for key in ["from", "size"] {
        if let Some(n) = as_i64(body_or_param(body, p, key)) {
            if n < 0 {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!("[{key}] parameter cannot be negative, found [{n}]"),
                ));
            }
        }
    }
    if let Some(n) = as_i64(p.get("batched_reduce_size").map(|v| json!(v))) {
        if n < 2 {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("batchedReduceSize must be >= 2, got {n}"),
            ));
        }
    }
    if let Some(n) = as_i64(p.get("pre_filter_shard_size").map(|v| json!(v))) {
        if n < 1 {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("preFilterShardSize must be >= 1, got {n}"),
            ));
        }
    }
    if let Some(n) = as_i64(
        body.get("track_total_hits").cloned().or_else(|| p.get("track_total_hits").map(|v| json!(v))),
    ) {
        if n < -1 {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("[track_total_hits] parameter must be positive or equals to -1, got {n}"),
            ));
        }
    }
    if let Some(st) = p.get("search_type") {
        if st == "query_and_fetch" || st == "dfs_query_and_fetch" {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("Unsupported search type [{st}]"),
            ));
        }
    }
    validate_params(body, p)?;
    let from = as_usize(body_or_param(body, p, "from")).unwrap_or(0);
    let size = as_usize(body_or_param(body, p, "size")).unwrap_or(10);
    // reading documents skips the closed indices a pattern would otherwise
    // reach; a closed index named outright is a different complaint
    // `pit` names a point in time rather than an index expression: it carries
    // both which indices to search and how far into each to look
    let pit = body
        .get("pit")
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .and_then(|id| store.read_pit(id));
    let expr: &str = match pit.as_ref() {
        Some(p) if expr.is_empty() => &p.expr,
        _ => expr,
    };
    let pit_ceiling: std::collections::HashMap<String, u64> =
        pit.as_ref().map(|p| p.ceiling.clone()).unwrap_or_default();
    let targets = store.resolve_open(expr);
    // `ignore_unavailable` says to pass over what cannot be searched rather
    // than to complain about it
    let lenient = p.get("ignore_unavailable").map(|v| v != "false").unwrap_or(false);
    // `expand_wildcards` naming closed indices means a pattern reaches them,
    // and a closed index cannot be searched whichever way it was reached
    let wants_closed = p
        .get("expand_wildcards")
        .map(|v| v.split(',').any(|w| matches!(w.trim(), "closed" | "all")))
        .unwrap_or(false);
    if wants_closed && !lenient {
        for name in store.resolve(expr) {
            if store.is_closed(&name) {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "index_closed_exception",
                    format!("closed index [{name}]"),
                ));
            }
        }
    }
    for name in expr
        .split(',')
        .map(|n| n.trim())
        .filter(|n| !n.is_empty() && !n.contains('*') && !lenient)
    {
        if store.is_closed(name) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "index_closed_exception",
                format!("closed index [{name}]"),
            ));
        }
    }
    if targets.is_empty()
        && !expr.contains('*')
        && expr != "_all"
        && !expr.is_empty()
        && !lenient
    {
        // a date-math name is reported as the index it stands for, since that
        // is the one that was not there
        return Err(no_such_index(&crate::store::resolve_date_math_name(expr)));
    }
    // `allow_no_indices=false` makes an expression that reaches nothing an
    // error rather than a search with nothing to search
    if targets.is_empty()
        && !expr.is_empty()
        && p.get("allow_no_indices").map(|v| v == "false").unwrap_or(false)
    {
        return Err(no_such_index(expr));
    }
    // a `terms` lookup names a document to read the term list from
    let mut query_json = body.get("query").cloned();
    if let Some(q) = query_json.as_mut() {
        if let Err(r) = resolve_terms_lookups(store, q) {
            return Err(r);
        }
        expand_bitmap_terms(q);
        expand_more_like_this(store, &targets, q);
    }

    // a field cannot be both kept and dropped: naming it in both lists asks
    // for two answers about the same field
    if let (Some(inc), Some(exc)) = (
        body.pointer("/_source/includes").and_then(|v| v.as_array()),
        body.pointer("/_source/excludes").and_then(|v| v.as_array()),
    ) {
        if let Some(both) = inc.iter().find(|i| exc.contains(i)).and_then(|v| v.as_str()) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!(
                    "The same entry [{both}] cannot be both included and excluded in _source."
                ),
            ));
        }
    }

    // `_shard_doc` orders by where a document sits within a shard, which only
    // holds still while a point-in-time is open; without one the order it
    // names does not exist
    if body.get("pit").is_none() {
        let names_shard_doc = |v: &Value| match v {
            Value::String(s) => s == "_shard_doc",
            Value::Object(o) => o.keys().any(|k| k == "_shard_doc"),
            _ => false,
        };
        let asked = match body.get("sort") {
            Some(Value::Array(a)) => a.iter().any(names_shard_doc),
            Some(one) => names_shard_doc(one),
            None => false,
        };
        if asked {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "action_request_validation_exception",
                "Validation Failed: 1: _shard_doc is only supported with point-in-time;",
            ));
        }
    }

    // unsigned_long cannot be sorted alongside another numeric type
    let sort_keys = parse_sort(body.get("sort"));
    for k in &sort_keys {
        let mut kinds: Vec<String> = Vec::new();
        for n in &targets {
            if let Some(st) = store.get(n) {
                if let Some(t) = st.read().mapping.type_of(&k.field) {
                    if !kinds.contains(&t.to_string()) {
                        kinds.push(t.to_string());
                    }
                }
            }
        }
        if kinds.len() > 1 && kinds.iter().any(|t| t == "unsigned_long") {
            return Err(err_caused_by(
                "search_phase_execution_exception",
                "all shards failed",
                "Can't do sort across indices, as a field has [unsigned_long] type in one index, \
                 and different type in another index!",
            ));
        }
    }
    let mut agg_json = body.get("aggs").or_else(|| body.get("aggregations")).cloned();
    // Parameter bounds do not depend on any mapping, so they are checked here
    // rather than per shard: a request that names no existing index has no
    // shards to walk, and a bad parameter would otherwise pass unread.
    if let Some(a) = agg_json.as_ref() {
        check_agg_bounds(a, "")?;
    }
    // buckets have to be weighted only where a document stands for several
    let weighted = targets
        .iter()
        .filter_map(|n| store.get(n))
        .any(|st| st.read().has_doc_count);
    if weighted {
        if let Some(a) = agg_json.as_mut() {
            inject_doc_count_helpers(a);
        }
    }
    if let Some(a) = agg_json.as_mut() {
        // a filter aggregation can carry a terms lookup too
        resolve_terms_lookups(store, a)?;
    }
    // tantivy has `filter` but not `filters`; peel those out and run them
    // ourselves as one filtered search per named bucket
    // sibling pipelines read the finished buckets, so they are held back and
    // computed once the rest of the aggregations have answered
    let mut pipeline_aggs: Vec<(String, Value)> = Vec::new();
    if let Some(Value::Object(o)) = agg_json.as_mut() {
        let names: Vec<String> =
            o.iter().filter(|(_, d)| is_pipeline_agg(d)).map(|(k, _)| k.clone()).collect();
        for n in names {
            if let Some(def) = o.remove(&n) {
                pipeline_aggs.push((n, def));
            }
        }
    }
    let mut filters_aggs: Vec<(String, Value)> = Vec::new();
    if let Some(Value::Object(o)) = agg_json.as_mut() {
        let names: Vec<String> = o
            .iter()
            .filter(|(_, def)| {
                def.get("filters").is_some()
                    || def.get("missing").is_some()
                    || def.get("median_absolute_deviation").is_some()
                    // HDR percentiles answer a different question from
                    // tantivy's t-digest, so they are computed here
                    || def
                        .get("percentiles")
                        .map(|v| v.get("hdr").is_some())
                        .unwrap_or(false)
                    // `_index` is metadata, not a column: bucket it ourselves
                    || def.get("global").is_some()
                    || def
                        .get("terms")
                        .and_then(|t| t.get("field"))
                        .and_then(|f| f.as_str())
                        == Some("_index")
                    // tantivy's own `filter` agg only speaks its query-string
                    // dialect, so run singular filters through our query builder
                    || def.get("filter").is_some()
                    || def.get("composite").is_some()
                    || def.get("multi_terms").is_some()
                    || def.get("rare_terms").is_some()
                    || def.get("ip_range").is_some()
                    || def.get("date_range").is_some()
                    || def.get("adjacency_matrix").is_some()
                    || def.get("weighted_avg").is_some()
                    || def.get("auto_date_histogram").is_some()
                    || def.get("variable_width_histogram").is_some()
                    // calendar units are not fixed lengths, which is all
                    // tantivy's date histogram knows how to step by
                    || def
                        .get("date_histogram")
                        .map(|d| d.get("calendar_interval").is_some())
                        .unwrap_or(false)
                    // a range field holds no single value to bucket a document
                    // by, so tantivy's histogram sees nothing there at all
                    || def
                        .get("histogram")
                        .and_then(|h| h.get("field"))
                        .and_then(|f| f.as_str())
                        .map(|f| range_field(store, &targets, f))
                        .unwrap_or(false)
                    // a field no document has, standing in for every document
                    || def
                        .get("terms")
                        .and_then(|t| t.get("field"))
                        .and_then(|f| f.as_str())
                        .map(|f| {
                            def.pointer("/terms/missing").is_some()
                                && unmapped_field(store, &targets, f)
                        })
                        .unwrap_or(false)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for n in names {
            if let Some(def) = o.remove(&n) {
                filters_aggs.push((n, def));
            }
        }
        if o.is_empty() {
            agg_json = None;
        }
    }
    // `fields` reads values back out of the stored source; without one there
    // is nothing to read, and a date format asks a field that holds no dates
    // to answer in a shape it has no values for
    if let Some(specs) = body.get("fields").and_then(|v| v.as_array()) {
        for name in targets.iter() {
            let Some(st) = store.get(name) else { continue };
            let g = st.read();
            if g.mapping.raw.pointer("/_source/enabled") == Some(&json!(false)) {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!(
                        "Unable to retrieve the requested [fields] since _source is disabled \
                         in the mappings for index [{name}]"
                    ),
                ));
            }
            for spec in specs {
                let (Some(f), Some(_)) = (
                    spec.get("field").and_then(|v| v.as_str()),
                    spec.get("format"),
                ) else {
                    continue;
                };
                if !matches!(
                    g.mapping.type_of(f),
                    None | Some("date" | "date_nanos" | "date_range")
                ) {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        format!("error fetching [{f}]: field has no date formatter"),
                    ));
                }
            }
        }
    }
    let source_sel = body.get("_source").cloned();
    // `fields` asks for values keyed by path, formatted, always as lists
    // `docvalue_fields` names the same values `fields` does; both are read out
    // of the stored source here, which holds every value either could report
    let spec_list = |v: Option<&Value>| -> Option<Vec<(String, Option<String>)>> {
        v.and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .filter_map(|x| match x {
                    Value::String(s) => Some((s.clone(), None)),
                    Value::Object(o) => o.get("field").and_then(|f| f.as_str()).map(|s| {
                        (
                            s.to_string(),
                            o.get("format").and_then(|f| f.as_str()).map(|s| s.to_string()),
                        )
                    }),
                    _ => None,
                })
                .collect()
        })
    };
    // `docvalue_fields` may also be named on the URL, as a comma-separated list
    let param_docvalues: Option<Value> = p.get("docvalue_fields").filter(|v| !v.is_empty()).map(|v| {
        Value::Array(v.split(',').map(|f| json!(f.trim())).collect())
    });
    let body_docvalues = body.get("docvalue_fields").cloned().or(param_docvalues);
    let field_specs: Option<Vec<(String, Option<String>)>> =
        match (spec_list(body.get("fields")), spec_list(body_docvalues.as_ref())) {
            (Some(mut a), Some(b)) => {
                a.extend(b);
                Some(a)
            }
            (a, b) => a.or(b),
        };
    let stored: Option<Vec<String>> = match body.get("stored_fields") {
        Some(Value::Array(a)) => {
            Some(a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        }
        Some(Value::String(s)) if s == "_none_" => Some(vec![]),
        Some(Value::String(s)) => Some(vec![s.clone()]),
        _ => None,
    };

    let started = std::time::Instant::now();
    let page_want = from + size;
    let mut cands: Vec<Cand> = Vec::new();
    let mut searchers: Vec<(String, Searcher, std::sync::Arc<parking_lot::RwLock<IdxState>>)> =
        Vec::new();
    let mut total: u64 = 0;
    let mut shards: u64 = 0;
    let mut empty_shards: u64 = 0;
    let mut agg_acc: Option<IntermediateAggregationResults> = None;
    let mut agg_req: Option<Aggregations> = None;
    let mut fruits: Vec<IntermediateAggregationResults> = Vec::new();
    let mut shard_profiles: Vec<Value> = Vec::new();
    let mut agg_meta: Vec<(String, Value)> = Vec::new();
    let mut bucket_orders: Vec<(String, String, bool)> = Vec::new();
    // which slice of the term space was asked for is a property of the
    // request, not of any one shard, so it is read once here
    let partitions: Vec<(String, i64, i64, usize)> = agg_json
        .clone()
        .map(|mut a| extract_partitions(&mut a))
        .unwrap_or_default();

    // One shard's work touches only its own index, so the fan-out runs across
    // cores. Searching many small indices is otherwise bounded by walking them
    // one at a time.
    struct ShardOut {
        name: String,
        searcher: Searcher,
        st: std::sync::Arc<parking_lot::RwLock<IdxState>>,
        shards: u64,
        count: usize,
        cands: Vec<Cand>,
        agg: Option<IntermediateAggregationResults>,
        agg_req: Option<Aggregations>,
        agg_meta: Vec<(String, Value)>,
        bucket_orders: Vec<(String, String, bool)>,
        profile: Option<Value>,
    }

    // `search_after` names where the previous page ended
    let search_after: Option<Vec<SortValue>> = body
        .get("search_after")
        .and_then(|v| v.as_array())
        .filter(|a| a.len() == sort_keys.len() && !a.is_empty())
        .map(|a| {
            a.iter()
                .zip(sort_keys.iter())
                .map(|(v, k)| sort_value_from_json(v, date_sort_key(store, &targets, &k.field)))
                .collect()
        });
    let fanned_out = targets.len() > 1;
    let run_shard = |shard_idx: usize, name: &String| -> std::result::Result<Option<ShardOut>, Response> {
        let Some(st) = store.get(name) else { return Ok(None) };
        let g = st.read();
        g.search_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if p.get("request_cache").map(|v| v == "true").unwrap_or(false) {
            g.request_cache_miss.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let mut shards = 0u64;
        let mut cands: Vec<Cand> = Vec::new();
        let mut agg_acc: Option<IntermediateAggregationResults> = None;
        let mut agg_req: Option<Aggregations> = None;
        let mut agg_meta: Vec<(String, Value)> = Vec::new();
        let mut bucket_orders: Vec<(String, String, bool)> = Vec::new();
        shards += g.shard_count();
        let ctx = Ctx {
            fields: &g.fields,
            mapping: &g.mapping,
            index: &g.index,
            max_terms_count: g.max_terms_count(),
            observed_kinds: &g.observed_kinds,
            kinds_complete: g.kinds_complete,
            stats: &g.stats,
        };
        let q: Box<dyn tantivy::query::Query> = match &query_json {
            Some(qj) => match crate::query::build(&ctx, qj) {
                Ok(q) => q,
                Err(e) => {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "parsing_exception",
                        e.to_string(),
                    ));
                }
            },
            None => Box::new(tantivy::query::AllQuery),
        };
        // a point in time holds the search to what the index had written when
        // it was opened, which is what makes paging through it stable
        let q: Box<dyn tantivy::query::Query> = match pit_ceiling.get(name) {
            Some(ceiling) => {
                let upper = tantivy::Term::from_field_u64(g.fields.seq, *ceiling);
                let below = tantivy::query::FastFieldRangeQuery::new(
                    std::ops::Bound::Unbounded,
                    std::ops::Bound::Excluded(upper),
                );
                Box::new(tantivy::query::BooleanQuery::new(vec![
                    (tantivy::query::Occur::Must, q),
                    (tantivy::query::Occur::Must, Box::new(below) as Box<dyn tantivy::query::Query>),
                ]))
            }
            None => q,
        };

        let searcher = g.reader.searcher();

        // the peeled aggregations never reach the parser, so their fields are
        // checked here rather than alongside the ones that do
        if !filters_aggs.is_empty() {
            let peeled: Value = filters_aggs
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<serde_json::Map<_, _>>()
                .into();
            check_agg_types(&peeled, &ctx)?;
        }

        // aggregations, when asked for, run over the same query
        let mut this_agg: Option<Aggregations> = None;
        let mut agg_request_json: Option<Value> = None;
        if let Some(aj) = &agg_json {
            let mut rewritten = aj.clone();
            normalize_aggs(&mut rewritten, &mut agg_meta, true);
            check_agg_types(&rewritten, &ctx)?;
            normalize_agg_dates(&mut rewritten);
            bucket_orders = extract_bucket_orders(&mut rewritten);
            let _ = extract_partitions(&mut rewritten);
            lower_nested_filters(&mut rewritten, &ctx);
            strip_untranslatable_term_filters(&mut rewritten, &ctx);
            rewrite_agg_fields(&mut rewritten, &ctx);
            agg_request_json = Some(rewritten.clone());
            match serde_json::from_value::<Aggregations>(rewritten) {
                Ok(a) => this_agg = Some(a),
                Err(e) => {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "x_content_parse_exception",
                        format!("failed to parse aggregation: {e}"),
                    ));
                }
            }
        }

        let want = page_want;
        // The aggregation rides along with the hit collection so the query is
        // walked once per index rather than twice. Profiling drives the phases
        // itself and keeps its own pass.
        let profiling = body.get("profile").map(|v| v == true).unwrap_or(false);
        let agg_collector = MaybeAgg(match (&this_agg, profiling) {
            (Some(a), false) => {
                let ctxp =
                    AggContextParams::new(Default::default(), g.index.tokenizers().clone());
                Some(DistributedAggregationCollector::from_aggs(a.clone(), ctxp))
            }
            _ => None,
        });

        let searched = if want == 0 {
            // `size: 0` asks for counts and aggregations only. Collecting a
            // page anyway means scoring and heap-ordering every match for a
            // result that is thrown away.
            search_shard(&searcher, &q, &(Count, agg_collector), fanned_out)
                .map(|(c, agg)| (c, Vec::new(), agg))
        } else if sort_keys.is_empty()
            && agg_collector.0.is_none()
            && count_without_walking(&query_json)
        {
            // Nothing else needs every document, so the top-k collector can
            // prune: once its heap is full, whole blocks that cannot beat the
            // worst kept score are skipped. Bundling a counter alongside it
            // would force every document to be visited and give that up --
            // measured at three to four times the throughput on this shape.
            //
            // The count then comes from the weight, which answers it from the
            // postings header for the queries that can, and otherwise walks
            // the same documents the tuple would have.
            // This query is cheap: the heap prunes and only `want` documents
            // are kept. Splitting its segments across the pool costs more in
            // coordination than the walk itself, and steals cores from the
            // aggregations, which are the expensive shape and do need them.
            let topk = search_shard(
                &searcher,
                &q,
                &TopDocs::with_limit(want.max(1)).order_by_score(),
                true,
            );
            topk.and_then(|docs| {
                let cands = docs
                    .into_iter()
                    .map(|(score, addr)| Cand {
                        shard: shard_idx,
                        addr,
                        score,
                        sort: Vec::new(),
                        seq: u64::MAX,
                    })
                    .collect::<Vec<_>>();
                let count = count_matches(&searcher, &q)?;
                Ok((count, cands, None))
            })
        } else if sort_keys.is_empty() {
            // an aggregation needs every document anyway, so there is nothing
            // to prune and hits ride along in the same pass
            let collector =
                (Count, TopDocs::with_limit(want.max(1)).order_by_score(), agg_collector);
            search_shard(&searcher, &q, &collector, fanned_out).map(|(c, docs, agg)| {
                let cands = docs
                    .into_iter()
                    .map(|(score, addr)| Cand {
                        shard: shard_idx,
                        addr,
                        score,
                        sort: Vec::new(),
                        seq: u64::MAX,
                    })
                    .collect::<Vec<_>>();
                (c, cands, agg)
            })
        } else {
            // sort keys are evaluated during collection, so only `want`
            // candidates are ever held rather than one per match
            let sources: Vec<SortSource> = sort_keys
                .iter()
                .map(|k| match k.field.as_str() {
                    "_score" => SortSource::Score,
                    "_doc" => SortSource::Doc,
                    _ => SortSource::Column {
                        name: ctx.column_name(&k.field, false),
                        desc: k.desc,
                        mode: k.mode.clone(),
                    },
                })
                .collect();
            let desc: Vec<bool> = sort_keys.iter().map(|k| k.desc).collect();
            let collector = (
                Count,
                SortCollector {
                    sources,
                    desc,
                    limit: want.max(1),
                    after: search_after.clone(),
                },
                agg_collector,
            );
            search_shard(&searcher, &q, &collector, fanned_out).map(|(c, mut cands, agg)| {
                for cand in cands.iter_mut() {
                    cand.shard = shard_idx;
                }
                (c, cands, agg)
            })
        };
        let (count, shard_cands, shard_agg) = match searched {
            Ok(v) => v,
            Err(e) => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "search_phase_execution_exception",
                    e.to_string(),
                ));
            }
        };
        if let Some(res) = shard_agg {
            agg_acc = Some(res);
            agg_req = this_agg.clone();
        }
        cands.extend(shard_cands);

        let mut shard_profile = None;
        if let (Some(a), true) = (this_agg, profiling) {
            let ctxp = AggContextParams::new(Default::default(), g.index.tokenizers().clone());
            let (res, prof) = profiled_agg_search(
                &searcher,
                &q,
                a.clone(),
                ctxp,
                &ctx,
                agg_request_json.as_ref(),
            );
            shard_profile = Some(prof);
            match res {
                Ok(res) => {
                    agg_acc = Some(res);
                    agg_req = Some(a);
                }
                Err(e) => {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "aggregation_execution_exception",
                        e.to_string(),
                    ));
                }
            }
        }

        Ok(Some(ShardOut {
            name: g.name.clone(),
            searcher,
            st: st.clone(),
            shards,
            count,
            cands,
            agg: agg_acc,
            agg_req,
            agg_meta,
            bucket_orders,
            profile: shard_profile,
        }))
    };

    let outs: Vec<std::result::Result<Option<ShardOut>, Response>> = if targets.len() > 1 {
        use rayon::prelude::*;
        targets
            .par_iter()
            .enumerate()
            .map(|(i, n)| run_shard(i, n))
            .collect()
    } else {
        targets.iter().enumerate().map(|(i, n)| run_shard(i, n)).collect()
    };

    for out in outs {
        let Some(o) = out? else { continue };
        shards += o.shards;
        total += o.count as u64;
        if o.count == 0 {
            empty_shards += 1;
        }
        cands.extend(o.cands);
        if let Some(res) = o.agg {
            fruits.push(res);
        }
        if o.agg_req.is_some() {
            agg_req = o.agg_req;
        }
        if agg_meta.is_empty() {
            agg_meta = o.agg_meta;
        }
        if bucket_orders.is_empty() {
            bucket_orders = o.bucket_orders;
        }
        if let Some(pr) = o.profile {
            shard_profiles.push(pr);
        }
        searchers.push((o.name, o.searcher, o.st));
    }

    // A wide fan-out leaves one intermediate result per index to combine.
    // Folding them one after another is linear and single-threaded, which at
    // a couple of hundred indices is a visible share of the whole request; a
    // tree reduction spreads it over the pool the shards already ran on.
    {
        agg_acc = if fruits.len() > 8 {
            use rayon::prelude::*;
            fruits.into_par_iter().reduce_with(|mut a, b| {
                let _ = a.merge_fruits(b);
                a
            })
        } else {
            fruits.into_iter().reduce(|mut a, b| {
                let _ = a.merge_fruits(b);
                a
            })
        };
    }

    prune(&mut cands, page_want, &sort_keys);
    // `indices_boost` weights whole indices against each other, so it is
    // applied to the scores before they are ranked. An alias may name the
    // index instead of the index naming itself.
    if let Some(boosts) = body.get("indices_boost") {
        let mut pairs: Vec<(String, f32)> = Vec::new();
        let mut take = |o: &serde_json::Map<String, Value>| {
            for (k, v) in o {
                if let Some(b) = v.as_f64() {
                    pairs.push((k.clone(), b as f32));
                }
            }
        };
        match boosts {
            Value::Object(o) => take(o),
            Value::Array(items) => {
                for item in items {
                    if let Some(o) = item.as_object() {
                        take(o);
                    }
                }
            }
            _ => {}
        }
        // a boost naming nothing is a request for an index that is not there,
        // unless the caller said to pass over what is missing
        let lenient = p.get("ignore_unavailable").map(|v| v != "false").unwrap_or(false);
        if !lenient {
            for (pat, _) in &pairs {
                let known = pat.contains('*')
                    || store.exists(pat)
                    || store.get(pat).is_some();
                if !known {
                    return Err(err(
                        StatusCode::NOT_FOUND,
                        "index_not_found_exception",
                        format!("no such index [{pat}]"),
                    ));
                }
            }
        }
        if !pairs.is_empty() {
            let names: Vec<String> = searchers.iter().map(|(n, _, _)| n.clone()).collect();
            let factor: Vec<f32> = names
                .iter()
                .map(|n| {
                    pairs
                        .iter()
                        .find(|(pat, _)| {
                            pat == n
                                || crate::store::glob_match(pat, n)
                                || store
                                    .get(pat)
                                    .map(|st| st.read().name == *n)
                                    .unwrap_or(false)
                        })
                        .map(|(_, b)| *b)
                        .unwrap_or(1.0)
                })
                .collect();
            for c in cands.iter_mut() {
                if let Some(f) = factor.get(c.shard) {
                    c.score *= f;
                }
            }
        }
    }

    fill_seq(&mut cands, &searchers);
    cands.sort_by(|a, b| cmp_cands(a, b, &sort_keys));

    // a score is only the best score when the ranking is by score descending;
    // any other order makes the top hit's score arbitrary
    let ranked_by_score = sort_keys.is_empty()
        || sort_keys.first().map(|k| k.field == "_score" && k.desc).unwrap_or(false);
    let max_score = if ranked_by_score {
        cands.iter().map(|c| c.score).fold(None::<f32>, |acc, s| Some(acc.map_or(s, |a| a.max(s))))
    } else {
        None
    };

    // `collapse` keeps one hit per distinct value of a field: the best one,
    // which after the sort is the first each value is seen at. It has to run
    // before the page is cut, or a page could be all one value's worth
    if let Some(field) = body.pointer("/collapse/field").and_then(|v| v.as_str()) {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let path = format!("/{}", field.replace('.', "/"));
        cands.retain(|c| {
            let (_, searcher, st) = &searchers[c.shard];
            let g = st.read();
            let value = source_of(searcher, &g, c.addr)
                .and_then(|(_, src)| src.pointer(&path).cloned())
                .map(|v| match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                });
            match value {
                Some(v) => seen.insert(v),
                // a document with no value there collapses with nothing
                None => true,
            }
        });
    }

    // now, and only now, read stored fields -- for at most `size` documents
    let mut all_hits: Vec<Hit> = Vec::new();
    for c in cands.into_iter().skip(from).take(size) {
        let (name, searcher, st) = &searchers[c.shard];
        let g = st.read();
        let Some((id, mut src)) = source_of(searcher, &g, c.addr) else { continue };
        // `_ignored` travels inside the stored source but belongs on the hit
        let ignored = src.as_object_mut().and_then(|o| o.remove("_ignored"));
        let version = g.version_of(&id);
        all_hits.push(Hit {
            seq: c.seq,
            shard_idx: c.shard,
            index: name.clone(),
            id,
            score: c.score,
            source: src,
            sort: c.sort,
            version,
            ignored,
        });
    }

    // Highlighting a long field means analysing it. Where the index caps how
    // much may be analysed, a field that exceeds the cap is refused rather
    // than silently truncated -- unless the request says how much to analyse,
    // or the field stores offsets and the highlighter can use them.
    if let Some(spec) = body.get("highlight") {
        for h in &all_hits {
            let g = searchers[h.shard_idx].2.read();
            let Some(cap) = g
                .setting("highlight.max_analyzed_offset")
                .and_then(|v| v.parse::<usize>().ok())
            else {
                break;
            };
            let plain = spec.get("type").and_then(|t| t.as_str()) == Some("plain");
            let Some(fields) = spec.get("fields").and_then(|f| f.as_object()) else { break };
            for (name, opts) in fields {
                if opts.get("max_analyzer_offset").is_some() {
                    continue;
                }
                let has_offsets = g
                    .mapping
                    .field_option(name, "index_options")
                    .and_then(|v| v.as_str().map(|s| s == "offsets"))
                    .unwrap_or(false)
                    || g.mapping.field_option(name, "term_vector").is_some();
                if has_offsets && !plain {
                    continue;
                }
                let too_long = h
                    .source
                    .pointer(&format!("/{}", name.replace('.', "/")))
                    .and_then(|v| v.as_str())
                    .map(|t| t.len() > cap)
                    .unwrap_or(false);
                if too_long {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        format!(
                            "The length of [{name}] field of [{}] doc of [{}] index has exceeded \
                             [{cap}] - maximum allowed to be analyzed for highlighting.",
                            h.id, h.index
                        ),
                    ));
                }
            }
        }
    }

    let suggest = match body.get("suggest") {
        Some(spec) => {
            let typed = p.get("typed_keys").map(|v| v != "false").unwrap_or(false);
            Some(build_suggest(store, &targets, spec, typed)?)
        }
        None => None,
    };

    let date_keys: Vec<bool> = sort_keys
        .iter()
        .map(|k| date_sort_key(store, &targets, &k.field))
        .collect();
    let page: Vec<Value> = all_hits
        .into_iter()
        .map(|h| {
            // `stored_fields: _none_` strips the metadata too
            let none = stored.as_ref().map(|s| s.is_empty()).unwrap_or(false)
                && body.get("stored_fields").map(|v| v == "_none_").unwrap_or(false);
            let mut hit = if none {
                json!({"_score": if sort_keys.is_empty() { json!(h.score) } else { Value::Null }})
            } else {
                json!({
                    "_index": h.index,
                    "_id": h.id,
                    "_score": if sort_keys.is_empty() { json!(h.score) } else { Value::Null },
                })
            };
            // a selector on the URL is the narrower instruction and wins over
            // one in the body
            let sel = crate::api::source_selector_from_params_pub(p).or_else(|| source_sel.clone());
            let explicit_source = sel.is_some();
            if let Some(names) = &stored {
                let mut out = serde_json::Map::new();
                for name in names.iter().filter(|n| *n != "_source") {
                    if let Some(v) = h.source.pointer(&format!("/{}", name.replace('.', "/"))) {
                        out.insert(
                            name.clone(),
                            match v {
                                Value::Array(a) => Value::Array(a.clone()),
                                other => Value::Array(vec![other.clone()]),
                            },
                        );
                    }
                }
                if !out.is_empty() {
                    hit["fields"] = Value::Object(out);
                }
            }
            // `stored_fields` suppresses `_source` unless it was asked for too
            let want_source = stored.is_none()
                || explicit_source
                || stored.as_ref().map(|s| s.iter().any(|n| n == "_source")).unwrap_or(false);
            if want_source {
                let src = match &sel {
                    Some(s) => apply_source_selector(&h.source, s),
                    None => h.source.clone(),
                };
                if !src.is_null() {
                    hit["_source"] = src;
                }
            }
            if let Some(ig) = &h.ignored {
                hit["_ignored"] = ig.clone();
            }
            if !h.sort.is_empty() {
                // a date column counts in nanoseconds; a sort value is
                // reported in milliseconds, as the field was written
                hit["sort"] = Value::Array(
                    h.sort
                        .iter()
                        .zip(date_keys.iter())
                        .map(|(s, is_date)| s.to_json_scaled(*is_date))
                        .collect(),
                );
            }
            if let Some(specs) = field_specs.as_ref() {
                let g = searchers[h.shard_idx].2.read();
                // a flat_object is one value unless the request named a path
                // inside it, in which case it has to be descended
                let is_leaf = |p: &str| {
                    g.mapping.is_leaf_type(p)
                        && !specs.iter().any(|(n, _)| {
                            n.len() > p.len() && n.starts_with(p) && n.as_bytes()[p.len()] == b'.'
                        })
                };
                // a field without doc values has nothing for `fields` to read
                let names: Vec<String> = specs
                    .iter()
                    .map(|(n, _)| n.clone())
                    .filter(|n| {
                        g.mapping.field_option(n, "doc_values") != Some(json!(false))
                    })
                    .collect();
                let mut f = crate::source::extract_fields(&h.source, &names, &is_leaf);
                // a format asks for the value written that way rather than
                // as the number it is
                for (name, fmt) in specs.iter() {
                    let Some(fmt) = fmt else { continue };
                    let Some(Value::Array(items)) = f.get_mut(name) else { continue };
                    for v in items.iter_mut() {
                        if let Some(n) = v.as_f64() {
                            if let Some(text) = decimal_format(fmt, n) {
                                *v = json!(text);
                            }
                        }
                    }
                }
                // a token_count field stores the text but reports the count
                for (name, vals) in f.iter_mut() {
                    if g.mapping.type_of(name) != Some("token_count") {
                        continue;
                    }
                    if let Value::Array(items) = vals {
                        for v in items.iter_mut() {
                            if let Some(t) = v.as_str() {
                                *v = json!(crate::store::token_count(t));
                            }
                        }
                    }
                }
                // a value the index refused is not a value the field has
                if let Some(Value::Array(ig)) = &h.ignored {
                    for name in ig.iter().filter_map(|v| v.as_str()) {
                        f.remove(name);
                    }
                }
                // apply any `format` the caller attached to a field
                for (name, fmt) in specs {
                    let Some(fmt) = fmt else { continue };
                    let Some(Value::Array(vals)) = f.get_mut(name) else { continue };
                    for v in vals.iter_mut() {
                        if let Some(formatted) = crate::source::format_date(v, fmt) {
                            *v = formatted;
                        }
                    }
                }
                // `stored_fields` may have filled some in already; both
                // selections share the one `fields` section
                if let Some(Value::Object(existing)) = hit.get("fields") {
                    for (k, v) in existing {
                        f.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
                if !f.is_empty() {
                    hit["fields"] = Value::Object(f);
                }
            }
            if let Some(spec) = body.get("highlight") {
                let g = searchers[h.shard_idx].2.read();
                if let Some(hl) =
                    build_highlight(spec, &h.source, &query_json, &g.mapping, &g.index)
                {
                    hit["highlight"] = hl;
                }
            }
            if body.get("version").and_then(|v| v.as_bool()).unwrap_or(false) {
                hit["_version"] = json!(h.version);
            }
            if body_or_param(body, p, "seq_no_primary_term")
                .map(|v| v == json!(true) || v == json!("true"))
                .unwrap_or(false)
            {
                hit["_seq_no"] = json!(h.seq);
                hit["_primary_term"] = json!(1);
            }
            hit
        })
        .collect();

    let mut filters_results: Vec<(String, Value)> = Vec::new();
    for (name, def) in &filters_aggs {
        let own_meta = def.get("meta").cloned();
        let outcome = run_peeled_agg(store, &targets, &query_json, name, def, weighted);
        match outcome {
            Ok(mut v) => {
                if let Some(m) = own_meta {
                    v["meta"] = m;
                }
                filters_results.push((name.clone(), v))
            }
            Err(r) => return Err(r),
        }
    }

    let aggs = match (agg_acc, agg_req) {
        (Some(acc), Some(req)) => match acc.into_final_result(req, Default::default()) {
            Ok(res) => serde_json::to_value(res).ok().map(|mut v| {
                recompute_extended_stats(&mut v);
                normalize_range_keys(&mut v);
                if let Some(req) = agg_json.as_ref() {
                    apply_bucket_formats(&mut v, req);
                    // a search may span indices, so a field's type is whatever
                    // the first index that names it says
                    let types: std::collections::HashMap<String, String> = targets
                        .iter()
                        .filter_map(|n| store.get(n))
                        .flat_map(|st| {
                            st.read().mapping.types.iter().map(|(k, t)| (k.clone(), t.clone())).collect::<Vec<_>>()
                        })
                        .collect();
                    format_terms_keys(&mut v, req, &types);
                    // one index may hold a field as whole numbers and another
                    // as fractions; the answer is one field, so the keys are
                    // written the wider way rather than two ways at once
                    let floating: std::collections::HashSet<String> = targets
                        .iter()
                        .filter_map(|n| store.get(n))
                        .flat_map(|st| {
                            let g = st.read();
                            g.observed_kinds
                                .iter()
                                .filter(|(_, k)| **k & crate::store::KIND_F64 != 0)
                                .map(|(f, _)| f.clone())
                                .collect::<Vec<_>>()
                        })
                        .collect();
                    widen_number_keys(&mut v, req, &floating);
                }
                if weighted {
                    apply_doc_counts(&mut v);
                }
                apply_bucket_orders(&mut v, &bucket_orders);
                apply_partitions(&mut v, &partitions);
                reattach_meta(&mut v, &agg_meta);
                v
            }),
            Err(e) => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "aggregation_execution_exception",
                    e.to_string(),
                ));
            }
        },
        _ => None,
    };

    let aggs = if filters_results.is_empty() {
        aggs
    } else {
        let mut base = aggs.unwrap_or_else(|| json!({}));
        for (name, v) in filters_results {
            base[name] = v;
        }
        Some(base)
    };

    // the profile is written while the aggregation runs, before there are any
    // buckets to count, so the count is filled in from the finished answer
    if let (Some(a), false) = (aggs.as_ref(), shard_profiles.is_empty()) {
        for shard in shard_profiles.iter_mut() {
            let Some(entries) = shard.get_mut("aggregations").and_then(|e| e.as_array_mut()) else {
                continue;
            };
            for entry in entries.iter_mut() {
                let Some(name) = entry.get("description").and_then(|d| d.as_str()) else { continue };
                let n = a
                    .get(name)
                    .and_then(|v| v.get("buckets"))
                    .and_then(|b| b.as_array())
                    .map(|b| b.len())
                    .unwrap_or(0);
                if let Some(debug) = entry.get_mut("debug").and_then(|d| d.as_object_mut()) {
                    debug.insert("total_buckets".into(), json!(n));
                }
            }
        }
    }

    let aggs = if pipeline_aggs.is_empty() {
        aggs
    } else {
        let mut base = aggs.unwrap_or_else(|| json!({}));
        for (name, def) in pipeline_aggs {
            base[name] = run_pipeline_agg(&base, &def)?;
        }
        Some(base)
    };

    // `search.max_buckets` caps how many buckets one request may build. The
    // limit is counted over the whole answer, sub-buckets included, which is
    // what makes a nested terms aggregation the expensive one.
    if let Some(limit) = store
        .cluster_setting("search.max_buckets")
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
    {
        fn count_buckets(node: &Value) -> u64 {
            match node {
                Value::Object(o) => o
                    .iter()
                    .map(|(k, v)| {
                        let here = if k == "buckets" {
                            match v {
                                Value::Array(a) => a.len() as u64,
                                Value::Object(b) => b.len() as u64,
                                _ => 0,
                            }
                        } else {
                            0
                        };
                        here + count_buckets(v)
                    })
                    .sum(),
                Value::Array(a) => a.iter().map(count_buckets).sum(),
                _ => 0,
            }
        }
        let built = aggs.as_ref().map(count_buckets).unwrap_or(0);
        if built > limit {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "too_many_buckets_exception",
                format!(
                    "Trying to create too many buckets. Must be less than or equal to: \
                     [{limit}] but was [{built}]. This limit can be set by changing the \
                     [search.max_buckets] cluster level setting."
                ),
            ));
        }
    }

    // pre-filtering lets a shard that cannot match be skipped entirely, but at
    // least one always runs so there is a real (empty) result to return
    // an aggregation that needs every shard (a `global`, or a bucket agg asking
    // for empty buckets) defeats pre-filtering
    fn needs_all_shards(node: &Value) -> bool {
        match node {
            Value::Object(o) => {
                o.contains_key("global")
                    || o.get("min_doc_count").and_then(|v| v.as_i64()) == Some(0)
                    || o.values().any(needs_all_shards)
            }
            Value::Array(a) => a.iter().any(needs_all_shards),
            _ => false,
        }
    }
    let agg_forces_all = body
        .get("aggs")
        .or_else(|| body.get("aggregations"))
        .map(needs_all_shards)
        .unwrap_or(false);

    let skipped = if p.contains_key("pre_filter_shard_size") && query_json.is_some()
        && !agg_forces_all
    {
        empty_shards.min((targets.len() as u64).saturating_sub(1))
    } else {
        0
    };

    Ok(Outcome {
        took_ms: started.elapsed().as_millis() as u64,
        skipped,
        shards: shards.max(1),
        total,
        hits: page,
        max_score,
        aggs,
        profile: (!shard_profiles.is_empty()).then(|| json!({"shards": shard_profiles})),
        suggest,
    })
}

/// Assemble the `hits` envelope, honouring track_total_hits and the
/// `rest_total_hits_as_int` compatibility switch.
pub fn envelope(out: Outcome, body: &Value, p: &Params) -> Value {
    let out_shards = out.shards;
    let out_skipped = out.skipped;
    let out_took = out.took_ms;
    let brs = p
        .get("batched_reduce_size")
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| body.get("batched_reduce_size").and_then(|v| v.as_u64()))
        .unwrap_or(512);
    let num_reduce_phases = if brs > 1 && out_shards > 1 {
        out_shards.saturating_sub(1).div_ceil(brs - 1)
    } else {
        1
    };
    let track = body.get("track_total_hits").cloned().or_else(|| {
        p.get("track_total_hits").map(|v| match v.as_str() {
            "true" => json!(true),
            "false" => json!(false),
            other => other.parse::<u64>().map(|n| json!(n)).unwrap_or(json!(true)),
        })
    });

    let mut hits_obj = json!({
        "max_score": out.max_score.map(|s| json!(s)).unwrap_or(Value::Null),
        "hits": out.hits,
    });

    let as_int = p.get("rest_total_hits_as_int").map(|v| v == "true").unwrap_or(false);
    let disabled = matches!(track, Some(Value::Bool(false)))
        || matches!(&track, Some(Value::String(s)) if s == "false");
    if disabled {
        // the int form reports -1 for "not tracked"; the object form is omitted
        if as_int {
            hits_obj["total"] = json!(-1);
        }
    } else {
        let limit = match &track {
            Some(Value::Bool(true)) => u64::MAX,
            Some(Value::Number(n)) => n.as_u64().unwrap_or(DEFAULT_TRACK_TOTAL_HITS),
            _ => DEFAULT_TRACK_TOTAL_HITS,
        };
        let (value, relation) =
            if out.total > limit { (limit, "gte") } else { (out.total, "eq") };
        hits_obj["total"] =
            if as_int { json!(value) } else { json!({"value": value, "relation": relation}) };
    }

    let mut resp = json!({
        "took": out_took,
        "timed_out": false,
        "_shards": {
            "total": out_shards, "successful": out_shards,
            "skipped": out_skipped, "failed": 0
        },
        "hits": hits_obj,
        "num_reduce_phases": num_reduce_phases,
    });
    if let Some(a) = out.aggs {
        resp["aggregations"] = a;
    }
    if let Some(pr) = out.profile {
        resp["profile"] = pr;
    }
    if let Some(sg) = out.suggest {
        resp["suggest"] = sg;
    }
    resp
}

pub fn owned_to_json(v: &OwnedValue) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}


/// Run one `filters` aggregation: a separate filtered search per bucket, with
/// the bucket's sub-aggregations evaluated inside it.
fn run_filters_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("filters").cloned().unwrap_or(json!({}));
    let inner = spec.get("filters").cloned().unwrap_or(Value::Null);
    let other_bucket_key = spec
        .get("other_bucket_key")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let other_bucket = spec.get("other_bucket").and_then(|v| v.as_bool()).unwrap_or(false)
        || other_bucket_key.is_some();
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();

    // named buckets keep their names; an array yields positional buckets
    let named: Vec<(Option<String>, Value)> = match &inner {
        Value::Object(o) => {
            if o.is_empty() {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    "[filters] cannot be empty",
                ));
            }
            o.iter().map(|(k, v)| (Some(k.clone()), v.clone())).collect()
        }
        Value::Array(a) => {
            if a.is_empty() {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    "[filters] cannot be empty",
                ));
            }
            a.iter().map(|v| (None, v.clone())).collect()
        }
        _ => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                "[filters] cannot be empty",
            ));
        }
    };

    let mut buckets: Vec<(Option<String>, Value)> = Vec::new();
    let mut matched_any: Vec<Value> = Vec::new();
    for (name, filter) in &named {
        let combined = combine(main_query, Some(filter.clone()));
        let (count, sub) = count_with_sub_aggs(store, targets, &combined, &sub_aggs, false)?;
        let mut b = json!({"doc_count": count});
        if let Some(sub) = sub {
            if let Some(o) = sub.as_object() {
                for (k, v) in o {
                    b[k] = v.clone();
                }
            }
        }
        buckets.push((name.clone(), b));
        matched_any.push(filter.clone());
    }

    if other_bucket {
        let none_of = json!({"bool": {"must_not": matched_any}});
        let combined = combine(main_query, Some(none_of));
        let (count, sub) = count_with_sub_aggs(store, targets, &combined, &sub_aggs, false)?;
        let mut b = json!({"doc_count": count});
        if let Some(sub) = sub {
            if let Some(o) = sub.as_object() {
                for (k, v) in o {
                    b[k] = v.clone();
                }
            }
        }
        buckets.push((Some(other_bucket_key.unwrap_or_else(|| "_other_".into())), b));
    }

    if buckets.iter().all(|(n, _)| n.is_some()) {
        let mut map = serde_json::Map::new();
        for (n, b) in buckets {
            map.insert(n.unwrap(), b);
        }
        Ok(json!({"buckets": Value::Object(map)}))
    } else {
        Ok(json!({"buckets": buckets.into_iter().map(|(_, b)| b).collect::<Vec<_>>()}))
    }
}

fn combine(main: &Option<Value>, extra: Option<Value>) -> Value {
    match (main, extra) {
        (Some(m), Some(e)) => json!({"bool": {"must": [m.clone()], "filter": [e]}}),
        (Some(m), None) => m.clone(),
        (None, Some(e)) => e,
        (None, None) => json!({"match_all": {}}),
    }
}

/// Run one aggregation that tantivy cannot parse itself.
///
/// These are computed by asking a question per bucket rather than by walking
/// the documents once, so any of them may appear inside any other: what a
/// bucket narrows to is just another query to ask the next one with.
fn run_peeled_agg(
    store: &Store,
    targets: &[String],
    query_json: &Option<Value>,
    name: &str,
    def: &Value,
    weighted: bool,
) -> std::result::Result<Value, Response> {
    if def.get("missing").is_some() {
        run_missing_agg(store, targets, query_json, def)
    } else if def.get("median_absolute_deviation").is_some() {
        run_mad_agg(store, targets, query_json, def)
    } else if def.get("percentiles").is_some() {
        run_hdr_percentiles(store, targets, query_json, def)
    } else if def.get("filter").is_some() {
        run_filter_agg(store, targets, query_json, def)
    } else if def.get("global").is_some() {
        // `global` ignores the query and aggregates over every document
        run_filter_agg(store, targets, &None, &json!({
            "filter": {"match_all": {}},
            "aggs": def.get("aggs").or_else(|| def.get("aggregations")).cloned()
                .unwrap_or_else(|| json!({}))
        }))
    } else if def.get("weighted_avg").is_some() {
        run_weighted_avg(store, targets, query_json, def)
    } else if def.get("variable_width_histogram").is_some() {
        run_variable_width_histogram(store, targets, query_json, def)
    } else if def.get("auto_date_histogram").is_some() {
        run_auto_date_histogram(store, targets, query_json, def)
    } else if def.get("date_range").is_some() {
        run_date_range_agg(store, targets, query_json, def)
    } else if def
        .get("terms")
        .and_then(|t| t.get("field"))
        .and_then(|f| f.as_str())
        .map(|f| {
            def.pointer("/terms/missing").is_some() && unmapped_field(store, targets, f)
        })
        .unwrap_or(false)
    {
        run_missing_terms_agg(store, targets, query_json, def)
    } else if def.get("histogram").is_some() {
        run_range_field_histogram(store, targets, query_json, def)
    } else if def.get("ip_range").is_some() {
        run_ip_range_agg(store, targets, query_json, def)
    } else if def.get("adjacency_matrix").is_some() {
        run_adjacency_matrix_agg(store, targets, query_json, def)
    } else if def.get("rare_terms").is_some() {
        run_rare_terms_agg(store, targets, query_json, def, weighted, name)
    } else if def.get("multi_terms").is_some() {
        run_multi_terms_agg(store, targets, query_json, def, weighted)
    } else if def.get("composite").is_some() {
        run_composite_agg(store, targets, query_json, def, weighted)
    } else if def.get("date_histogram").is_some() {
        run_calendar_histogram(store, targets, query_json, def)
    } else if def.get("terms").is_some() {
        run_index_terms_agg(store, targets, query_json, def)
    } else {
        run_filters_agg(store, targets, query_json, def)
    }
}

/// Split sub-aggregations into the ones this engine computes itself and the
/// ones tantivy can parse, so each set can take the path that suits it.
fn split_peelable(sub_aggs: &Option<Value>) -> (Option<Value>, Option<Value>) {
    let Some(o) = sub_aggs.as_ref().and_then(|s| s.as_object()) else {
        return (None, sub_aggs.clone());
    };
    let (mine, theirs): (Vec<_>, Vec<_>) = o.iter().partition(|(_, d)| peelable(d));
    let pack = |v: Vec<(&String, &Value)>| {
        if v.is_empty() {
            None
        } else {
            Some(Value::Object(v.into_iter().map(|(k, d)| (k.clone(), d.clone())).collect()))
        }
    };
    (pack(mine), pack(theirs))
}

/// Is this an aggregation tantivy has no parser for, which has to be computed
/// a bucket at a time here instead?
fn peelable(def: &Value) -> bool {
    const OWN: &[&str] = &[
        "missing", "median_absolute_deviation", "filter", "global", "weighted_avg",
        "variable_width_histogram", "auto_date_histogram", "date_range", "ip_range",
        "adjacency_matrix", "rare_terms", "multi_terms", "composite",
    ];
    OWN.iter().any(|k| def.get(k).is_some())
        || def
            .get("date_histogram")
            .map(|d| d.get("calendar_interval").is_some())
            .unwrap_or(false)
}

/// Count the documents a query matches, and run its sub-aggregations --
/// including the ones tantivy cannot parse, which are run here against the
/// same query rather than handed down.
fn count_with_sub_aggs(
    store: &Store,
    targets: &[String],
    query_json: &Value,
    sub_aggs: &Option<Value>,
    weighted: bool,
) -> std::result::Result<(u64, Option<Value>), Response> {
    let Some(subs) = sub_aggs.as_ref().and_then(|s| s.as_object()) else {
        return filtered_count(store, targets, query_json, sub_aggs);
    };
    let (mine, theirs): (Vec<_>, Vec<_>) =
        subs.iter().partition(|(_, d)| peelable(d));
    if mine.is_empty() {
        return filtered_count(store, targets, query_json, sub_aggs);
    }
    let rest: Option<Value> = if theirs.is_empty() {
        None
    } else {
        Some(Value::Object(theirs.into_iter().map(|(k, v)| (k.clone(), v.clone())).collect()))
    };
    let (count, mut out) = filtered_count(store, targets, query_json, &rest)?;
    let base = Some(query_json.clone());
    let mut merged = out.take().and_then(|v| v.as_object().cloned()).unwrap_or_default();
    for (n, d) in mine {
        merged.insert(n.clone(), run_peeled_agg(store, targets, &base, n, d, weighted)?);
    }
    Ok((count, Some(Value::Object(merged))))
}

fn filtered_count(
    store: &Store,
    targets: &[String],
    query_json: &Value,
    sub_aggs: &Option<Value>,
) -> std::result::Result<(u64, Option<Value>), Response> {
    let mut total = 0u64;
    let mut acc: Option<IntermediateAggregationResults> = None;
    let mut req: Option<Aggregations> = None;
    for name in targets {
        let Some(st) = store.get(name) else { continue };
        let g = st.read();
        let ctx = Ctx {
            fields: &g.fields,
            mapping: &g.mapping,
            index: &g.index,
            max_terms_count: g.max_terms_count(),
            observed_kinds: &g.observed_kinds,
            kinds_complete: g.kinds_complete,
            stats: &g.stats,
        };
        let q = crate::query::build(&ctx, query_json)
            .map_err(|e| err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string()))?;
        let searcher = g.reader.searcher();
        total += searcher
            .search(&q, &Count)
            .map_err(|e| err(StatusCode::BAD_REQUEST, "search_phase_execution_exception", e.to_string()))?
            as u64;

        if let Some(sa) = sub_aggs {
            let mut rewritten = sa.clone();
            let mut ignored = Vec::new();
            normalize_aggs(&mut rewritten, &mut ignored, false);
            rewrite_agg_fields(&mut rewritten, &ctx);
            let parsed: Aggregations = serde_json::from_value(rewritten).map_err(|e| {
                err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string())
            })?;
            let ctxp = AggContextParams::new(Default::default(), g.index.tokenizers().clone());
            let res = searcher
                .search(&q, &DistributedAggregationCollector::from_aggs(parsed.clone(), ctxp))
                .map_err(|e| {
                    err(StatusCode::BAD_REQUEST, "aggregation_execution_exception", e.to_string())
                })?;
            match acc.as_mut() {
                Some(a) => {
                    let _ = a.merge_fruits(res);
                }
                None => acc = Some(res),
            }
            req = Some(parsed);
        }
    }
    let sub = match (acc, req) {
        (Some(a), Some(r)) => a.into_final_result(r, Default::default()).ok().and_then(|v| serde_json::to_value(v).ok()),
        _ => None,
    };
    Ok((total, sub))
}


/// `missing`: a single bucket of documents that have no value for the field.
fn run_missing_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let field = def
        .get("missing")
        .and_then(|m| m.get("field"))
        .and_then(|f| f.as_str())
        .unwrap_or_default()
        .to_string();
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    // a `missing` default fills every gap, so the bucket is empty by definition
    if def.get("missing").and_then(|m| m.get("missing")).is_some() {
        return Ok(json!({"doc_count": 0}));
    }
    let absent = json!({"bool": {"must_not": [{"exists": {"field": field}}]}});
    let combined = combine(main_query, Some(absent));
    let (count, sub) = count_with_sub_aggs(store, targets, &combined, &sub_aggs, false)?;
    let mut out = json!({"doc_count": count});
    if let Some(sub) = sub {
        if let Some(o) = sub.as_object() {
            for (k, v) in o {
                out[k] = v.clone();
            }
        }
    }
    Ok(out)
}


/// `filter`: one bucket holding the documents that match a sub-query.
fn run_filter_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let filter = def.get("filter").cloned().unwrap_or(json!({"match_all": {}}));
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let combined = combine(main_query, Some(filter));
    let (count, sub) = count_with_sub_aggs(store, targets, &combined, &sub_aggs, false)?;
    let mut out = json!({"doc_count": count});
    if let Some(sub) = sub {
        if let Some(o) = sub.as_object() {
            for (k, v) in o {
                out[k] = v.clone();
            }
        }
    }
    Ok(out)
}


/// `terms` over the `_index` metadata field: one bucket per index that has hits.
fn run_index_terms_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let size = def
        .get("terms")
        .and_then(|t| t.get("size"))
        .and_then(|v| v.as_u64())
        .unwrap_or(10) as usize;
    let min_doc_count = def
        .get("terms")
        .and_then(|t| t.get("min_doc_count"))
        .and_then(|v| v.as_u64())
        .unwrap_or(1);
    let query = combine(main_query, None);
    let mut buckets = Vec::new();
    for name in targets {
        let (count, sub) = filtered_count(store, std::slice::from_ref(name), &query, &sub_aggs)?;
        if count < min_doc_count {
            continue;
        }
        let mut b = json!({"key": name, "doc_count": count});
        if let Some(sub) = sub.and_then(|v| v.as_object().cloned()) {
            for (k, v) in sub {
                b[k] = v;
            }
        }
        buckets.push(b);
    }
    buckets.sort_by(|a, b| {
        let ad = a["doc_count"].as_u64().unwrap_or(0);
        let bd = b["doc_count"].as_u64().unwrap_or(0);
        bd.cmp(&ad).then_with(|| a["key"].as_str().cmp(&b["key"].as_str()))
    });
    buckets.truncate(size);
    Ok(json!({
        "doc_count_error_upper_bound": 0,
        "sum_other_doc_count": 0,
        "buckets": buckets
    }))
}


/// Every value of one numeric field across the documents a query matches.
///
/// Aggregations that tantivy does not provide are computed from these directly;
/// the field is read from the columnar, so nothing is materialised per document
/// beyond the value itself.
fn collect_field_values(
    store: &Store,
    targets: &[String],
    query_json: &Value,
    field: &str,
    missing: Option<f64>,
) -> std::result::Result<Vec<f64>, Response> {
    let mut out = Vec::new();
    for name in targets {
        let Some(st) = store.get(name) else { continue };
        let g = st.read();
        let ctx = Ctx {
            fields: &g.fields,
            mapping: &g.mapping,
            index: &g.index,
            max_terms_count: g.max_terms_count(),
            observed_kinds: &g.observed_kinds,
            kinds_complete: g.kinds_complete,
            stats: &g.stats,
        };
        let q = crate::query::build(&ctx, query_json)
            .map_err(|e| err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string()))?;
        let column = ctx.column_name(field, false);
        let searcher = g.reader.searcher();
        let addrs = searcher
            .search(&q, &tantivy::collector::DocSetCollector)
            .map_err(|e| err(StatusCode::BAD_REQUEST, "search_phase_execution_exception", e.to_string()))?;
        let cols: Vec<SortColumns> = searcher
            .segment_readers()
            .iter()
            .map(|r| SortColumns::for_segment(r, &column))
            .collect();
        for addr in addrs {
            let Some(c) = cols.get(addr.segment_ord as usize) else { continue };
            let mut any = false;
            for v in c.numeric_values(addr.doc_id) {
                out.push(v);
                any = true;
            }
            if !any {
                if let Some(m) = missing {
                    out.push(m);
                }
            }
        }
    }
    Ok(out)
}

fn agg_field_and_missing(spec: &Value) -> (String, Option<f64>) {
    let field = spec.get("field").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let missing = spec.get("missing").and_then(|v| v.as_f64());
    (field, missing)
}

/// `percentiles` with an `hdr` option, reported the way HdrHistogram does.
fn run_hdr_percentiles(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("percentiles").cloned().unwrap_or(json!({}));
    if let Some(digits) = spec.pointer("/hdr/number_of_significant_value_digits") {
        let d = digits.as_i64().unwrap_or(3);
        if !(0..=5).contains(&d) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("[numberOfSignificantValueDigits] must be between 0 and 5: [{d}]"),
            ));
        }
    }
    let (field, missing) = agg_field_and_missing(&spec);
    let percents: Vec<f64> = spec
        .get("percents")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_f64()).collect())
        .unwrap_or_else(|| vec![1.0, 5.0, 25.0, 50.0, 75.0, 95.0, 99.0]);
    let keyed = spec.get("keyed").and_then(|v| v.as_bool()).unwrap_or(true);

    let query = combine(main_query, None);
    let values = collect_field_values(store, targets, &query, &field, missing)?;
    let mut hist = crate::hdr::HdrHistogram::default();
    for v in &values {
        hist.record(*v);
    }

    if keyed {
        let mut map = serde_json::Map::new();
        for p in &percents {
            let key = format!("{:.1}", p);
            map.insert(key, hist.value_at(*p).map(|v| json!(v)).unwrap_or(Value::Null));
        }
        Ok(json!({ "values": Value::Object(map) }))
    } else {
        let arr: Vec<Value> = percents
            .iter()
            .map(|p| json!({"key": p, "value": hist.value_at(*p)}))
            .collect();
        Ok(json!({ "values": arr }))
    }
}


/// A date histogram stepped by calendar units.
///
/// A month is not a fixed number of milliseconds, so tantivy's histogram --
/// which steps by a constant -- cannot express one. Each bucket is instead a
/// range filter run through the ordinary query path, which also means
/// sub-aggregations come for free. The cost is one search per bucket, which
/// suits the handful of buckets a calendar histogram usually spans.




/// Does this field's terms live somewhere include/exclude cannot reach?
///
/// Both are matched against the term dictionary. An address is in there, but
/// as the fixed-width form rather than as it was written; a date is not in
/// there at all, since a date column is numeric. Either way the filter has to
/// come off the request and be applied to the answer instead.
fn term_filter_needs_translating(ty: Option<&str>) -> bool {
    matches!(ty, Some("ip" | "date" | "date_nanos"))
}

/// Take include/exclude off the aggregations whose field cannot honour them.
fn strip_untranslatable_term_filters(node: &mut Value, ctx: &Ctx) {
    match node {
        Value::Object(o) => {
            if let Some(terms) = o.get_mut("terms").and_then(|t| t.as_object_mut()) {
                let field = terms.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
                let base = field.strip_suffix(".keyword").unwrap_or(&field);
                if term_filter_needs_translating(ctx.mapping.type_of(base)) {
                    terms.remove("include");
                    terms.remove("exclude");
                }
            }
            for (_, v) in o.iter_mut() {
                strip_untranslatable_term_filters(v, ctx);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|v| strip_untranslatable_term_filters(v, ctx)),
        _ => {}
    }
}

/// Render bucket keys through the `format` an aggregation asked for.
///
/// The pattern is Java's decimal format. Only the shape that appears in
/// practice is handled -- literal text around a run of `#` and `0`, where the
/// zeros after the point set how many decimals to show -- rather than the
/// whole grammar.
fn apply_bucket_formats(result: &mut Value, req: &Value) {
    let Some(reqo) = req.as_object() else { return };
    for (name, def) in reqo {
        let Some(defo) = def.as_object() else { continue };
        let Some(node) = result.get_mut(name) else { continue };
        let format = defo
            .values()
            .next()
            .and_then(|body| body.get("format"))
            .and_then(|f| f.as_str())
            .map(|s| s.to_string());
        if let (Some(fmt), Some(Value::Array(buckets))) = (&format, node.get_mut("buckets")) {
            for b in buckets.iter_mut() {
                let Some(o) = b.as_object_mut() else { continue };
                let Some(n) = o.get("key").and_then(|k| k.as_f64()) else { continue };
                if let Some(text) = decimal_format(fmt, n) {
                    o.insert("key_as_string".into(), Value::String(text));
                }
            }
        }
        let Some(sub) = defo.get("aggs").or_else(|| defo.get("aggregations")) else { continue };
        match node.get_mut("buckets") {
            Some(Value::Array(buckets)) => {
                for b in buckets.iter_mut() {
                    apply_bucket_formats(b, sub);
                }
            }
            _ => apply_bucket_formats(node, sub),
        }
    }
}

/// `Value is ##0.0` applied to 50 gives `Value is 50.0`.
fn decimal_format(pattern: &str, value: f64) -> Option<String> {
    let start = pattern.find(['#', '0'])?;
    let end = pattern.rfind(['#', '0'])? + 1;
    let (prefix, numeric, suffix) = (&pattern[..start], &pattern[start..end], &pattern[end..]);
    let decimals = match numeric.split_once('.') {
        Some((_, frac)) => frac.chars().filter(|c| *c == '0').count(),
        None => 0,
    };
    Some(format!("{prefix}{value:.decimals$}{suffix}"))
}

/// Write each `terms` bucket key in the spelling its field is read in.
///
/// An address is stored in the fixed-width form that sorts correctly and a
/// date as text; neither is what the field was given, so the request is walked
/// alongside the answer to find which field each set of buckets came from.
/// Write a terms aggregation's numeric keys as fractions when any index in
/// the search holds the field that way.
///
/// Two indices can disagree: one stores whole numbers, the other fractions.
/// The buckets merge on value regardless, but a key written back as `10` where
/// another document contributed `10.0` reports a field that has two types.
/// Ties in a count are settled by key, which is the order that produces.
fn widen_number_keys(
    result: &mut Value,
    req: &Value,
    floating: &std::collections::HashSet<String>,
) {
    let Some(reqo) = req.as_object() else { return };
    for (name, def) in reqo {
        let Some(defo) = def.as_object() else { continue };
        let Some(node) = result.get_mut(name) else { continue };
        if let Some(sub) = defo.get("aggs").or_else(|| defo.get("aggregations")) {
            widen_number_keys(node, sub, floating);
        }
        let Some(terms) = defo.get("terms") else { continue };
        let field = terms.get("field").and_then(|f| f.as_str()).unwrap_or("");
        if !floating.contains(field) {
            continue;
        }
        let Some(Value::Array(buckets)) = node.get_mut("buckets") else { continue };
        for b in buckets.iter_mut() {
            let Some(o) = b.as_object_mut() else { continue };
            let widened = o.get("key").and_then(|k| k.as_i64()).map(|i| i as f64);
            if let Some(f) = widened {
                o.insert("key".into(), json!(f));
            }
        }
        // an explicit order is the caller's, and is left alone
        if terms.get("order").is_some() {
            continue;
        }
        buckets.sort_by(|a, b| {
            let count = |v: &Value| v.get("doc_count").and_then(|c| c.as_u64()).unwrap_or(0);
            let key = |v: &Value| v.get("key").and_then(|k| k.as_f64()).unwrap_or(f64::MAX);
            count(b).cmp(&count(a)).then(
                key(a).partial_cmp(&key(b)).unwrap_or(Ordering::Equal),
            )
        });
    }
}

fn format_terms_keys(
    result: &mut Value,
    req: &Value,
    types: &std::collections::HashMap<String, String>,
) {
    let Some(reqo) = req.as_object() else { return };
    for (name, def) in reqo {
        let Some(defo) = def.as_object() else { continue };
        let Some(node) = result.get_mut(name) else { continue };

        if defo.contains_key("terms") {
            let field = defo
                .get("terms")
                .and_then(|t| t.get("field"))
                .and_then(|f| f.as_str())
                .unwrap_or("");
            let base = field.strip_suffix(".keyword").unwrap_or(field);
            let ty = types.get(base).cloned();
            let listed = |key: &str| -> Option<Vec<String>> {
                let v = defo.get("terms")?.get(key)?;
                Some(match v {
                    Value::Array(a) => a.iter().filter_map(term_filter_text).collect(),
                    other => term_filter_text(other).into_iter().collect(),
                })
            };
            let translating = term_filter_needs_translating(ty.as_deref());
            let include = translating.then(|| listed("include")).flatten();
            let exclude = translating.then(|| listed("exclude")).flatten();

            if let Some(Value::Array(buckets)) = node.get_mut("buckets") {
                for b in buckets.iter_mut() {
                    let Some(o) = b.as_object_mut() else { continue };
                    let Some(raw) = o.get("key").cloned() else { continue };
                    let (key, as_string) = terms_key_view(raw, ty.as_deref());
                    o.insert("key".into(), key);
                    match as_string {
                        Some(text) => {
                            o.insert("key_as_string".into(), Value::String(text));
                        }
                        None => {
                            o.remove("key_as_string");
                        }
                    }
                }
                // the filters that could not be pushed down are applied here
                if include.is_some() || exclude.is_some() {
                    buckets.retain(|b| {
                        let shown = (
                            b.get("key").cloned().unwrap_or(Value::Null),
                            b.get("key_as_string")
                                .and_then(|s| s.as_str())
                                .map(|s| s.to_string()),
                        );
                        let hit = |list: &Vec<String>| {
                            list.iter()
                                .any(|want| term_filter_matches(want, &shown, ty.as_deref()))
                        };
                        include.as_ref().map(|l| hit(l)).unwrap_or(true)
                            && !exclude.as_ref().map(|l| hit(l)).unwrap_or(false)
                    });
                }
            }
        }

        let Some(sub) = defo.get("aggs").or_else(|| defo.get("aggregations")) else { continue };
        match node.get_mut("buckets") {
            Some(Value::Array(buckets)) => {
                for b in buckets.iter_mut() {
                    format_terms_keys(b, sub, types);
                }
            }
            Some(Value::Object(keyed)) => {
                for (_, b) in keyed.iter_mut() {
                    format_terms_keys(b, sub, types);
                }
            }
            _ => format_terms_keys(node, sub, types),
        }
    }
}

/// A numeric range bucket names its bounds as doubles.
///
/// tantivy writes `*-50` where the suite expects `*-50.0`; the bounds are
/// already on the bucket, so the key is rebuilt from them rather than parsed.
fn normalize_range_keys(node: &mut Value) {
    match node {
        Value::Object(o) => {
            if let Some(Value::Array(buckets)) = o.get_mut("buckets") {
                for b in buckets.iter_mut() {
                    let numeric = b.get("from").map(|v| v.is_number()).unwrap_or(false)
                        || b.get("to").map(|v| v.is_number()).unwrap_or(false);
                    let has_key = b.get("key").map(|k| k.is_string()).unwrap_or(false);
                    if !numeric || !has_key {
                        continue;
                    }
                    let show = |v: Option<&Value>| match v.and_then(|x| x.as_f64()) {
                        Some(n) if n.is_finite() => {
                            if n.fract() == 0.0 && n.abs() < 1e15 {
                                format!("{n:.1}")
                            } else {
                                format!("{n}")
                            }
                        }
                        _ => "*".to_string(),
                    };
                    let key = format!("{}-{}", show(b.get("from")), show(b.get("to")));
                    b["key"] = json!(key);
                }
            }
            for (_, v) in o.iter_mut() {
                normalize_range_keys(v);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(normalize_range_keys),
        _ => {}
    }
}


/// `date_range`: one bucket per span of time.
///
/// Each range becomes a filter on the field, so the ordinary query path
/// answers it. The bounds are reported in epoch milliseconds however they were
/// written, while the key keeps the caller's own spelling.
fn run_date_range_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("date_range").cloned().unwrap_or(json!({}));
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let keyed = spec.get("keyed").and_then(|v| v.as_bool()).unwrap_or(false);
    let missing = spec.get("missing").cloned();

    // the request may name its own format; otherwise the mapping's applies
    let mapped_format = targets
        .iter()
        .filter_map(|n| store.get(n))
        .next()
        .and_then(|st| {
            st.read().mapping.field_option(&field, "format").and_then(|v| {
                v.as_str().map(|s| s.to_string())
            })
        });
    let format = spec
        .get("format")
        .and_then(|f| f.as_str())
        .map(|s| s.to_string())
        .or(mapped_format);

    let iso = |v: &Value| crate::store::canonical_date_with(v, format.as_deref());
    let millis = |v: &Value| {
        iso(v)
            .and_then(|s| crate::store::parse_date_lenient(&s))
            .map(|d| (d.unix_timestamp_nanos() / 1_000_000) as i64)
    };
    // a bound is named in the key the way it is reported beside it, not the
    // way the request happened to spell it
    let shown = |v: &Option<Value>| match v {
        // a bound written as a date is named in the key the way it is
        // reported beside it; one written as a number is a number
        Some(Value::String(s)) => iso(&json!(s))
            .and_then(|t| crate::store::parse_date_lenient(&t))
            .map(iso_millis)
            .unwrap_or_else(|| s.clone()),
        Some(other) if !other.is_null() => other.to_string(),
        _ => "*".to_string(),
    };

    let mut buckets = Vec::new();
    let mut keyed_out = serde_json::Map::new();
    for range in spec.get("ranges").and_then(|r| r.as_array()).into_iter().flatten() {
        let from = range.get("from").cloned().filter(|v| !v.is_null());
        let to = range.get("to").cloned().filter(|v| !v.is_null());
        let mut clause = serde_json::Map::new();
        if let Some(f) = from.as_ref().and_then(iso) {
            clause.insert("gte".into(), json!(f));
        }
        if let Some(t) = to.as_ref().and_then(iso) {
            clause.insert("lt".into(), json!(t));
        }
        let unbounded = clause.is_empty();
        let filter = if unbounded {
            // documents with no value take part when a stand-in was named
            if missing.is_some() {
                json!({"match_all": {}})
            } else {
                json!({"exists": {"field": field}})
            }
        } else {
            json!({"range": {field.clone(): Value::Object(clause)}})
        };
        let combined = combine(main_query, Some(filter));
        let (count, sub) = count_with_sub_aggs(store, targets, &combined, &sub_aggs, false)?;

        let key = range
            .get("key")
            .and_then(|k| k.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{}-{}", shown(&from), shown(&to)));
        let mut b = json!({"key": key.clone(), "doc_count": count});
        if let Some(f) = from.as_ref() {
            if let Some(ms) = millis(f) {
                b["from"] = json!(ms);
                if let Some(s) = iso(f) {
                    b["from_as_string"] = json!(s);
                }
            }
        }
        if let Some(t) = to.as_ref() {
            if let Some(ms) = millis(t) {
                b["to"] = json!(ms);
                if let Some(s) = iso(t) {
                    b["to_as_string"] = json!(s);
                }
            }
        }
        if let Some(Value::Object(o)) = sub {
            for (k, v) in o {
                b[k] = v;
            }
        }
        if keyed {
            keyed_out.insert(key, b);
        } else {
            buckets.push(b);
        }
    }
    if keyed {
        return Ok(json!({"buckets": Value::Object(keyed_out)}));
    }
    Ok(json!({"buckets": buckets}))
}

/// `ip_range`: one bucket per address range.
///
/// Each range is a filter on the field, so the ordinary query path answers it;
/// `from` is included and `to` is not, and either may be left open.
/// Is this a field no index in the search knows anything about -- neither
/// mapped nor ever seen in a document?
fn unmapped_field(store: &Store, targets: &[String], field: &str) -> bool {
    !targets.iter().filter_map(|n| store.get(n)).any(|st| {
        let g = st.read();
        g.mapping.type_of(field).is_some() || g.observed_kinds.contains_key(field)
    })
}

/// A terms aggregation over a field no document has.
///
/// With `missing` given, every document takes that one value, so there is one
/// bucket holding all of them. `value_type` says how the key is written --
/// what the field would have been, had anything ever put a value in it.
fn run_missing_terms_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("terms").cloned().unwrap_or_else(|| json!({}));
    let missing = spec.get("missing").cloned().unwrap_or(Value::Null);
    let base = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let (count, sub) = count_with_sub_aggs(store, targets, &base, &sub_aggs, false)?;
    if count == 0 {
        return Ok(json!({
            "doc_count_error_upper_bound": 0, "sum_other_doc_count": 0, "buckets": []
        }));
    }
    let ty = spec.get("value_type").and_then(|v| v.as_str()).unwrap_or("");
    let (key, as_string) = match ty {
        "boolean" => {
            let b = match &missing {
                Value::Bool(b) => *b,
                Value::String(s) => s == "true",
                Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
                _ => false,
            };
            (json!(if b { 1 } else { 0 }), Some(b.to_string()))
        }
        "date" => {
            let text = missing.as_str().unwrap_or_default().to_string();
            match crate::store::canonical_date(&missing)
                .and_then(|d| crate::store::parse_date_lenient(&d))
            {
                Some(d) => {
                    let ms = (d.unix_timestamp_nanos() / 1_000_000) as i64;
                    (json!(ms), Some(iso_millis(d)))
                }
                None => (json!(text), None),
            }
        }
        _ => (missing.clone(), None),
    };
    let mut bucket = json!({"key": key, "doc_count": count});
    if let Some(text) = as_string {
        bucket["key_as_string"] = json!(text);
    }
    if let Some(Value::Object(o)) = sub {
        for (k, v) in o {
            bucket[k] = v;
        }
    }
    Ok(json!({
        "doc_count_error_upper_bound": 0,
        "sum_other_doc_count": 0,
        "buckets": [bucket],
    }))
}

/// Is this field one of the range types, which store two endpoints per
/// document rather than one value?
fn range_field(store: &Store, targets: &[String], field: &str) -> bool {
    targets.iter().filter_map(|n| store.get(n)).any(|st| {
        st.read().mapping.type_of(field).map(|t| t.ends_with("_range")).unwrap_or(false)
    })
}

/// A numeric histogram over a range field.
///
/// A range document has no single value to fall into one bucket; it covers a
/// span, and belongs to every bucket that span touches. So each bucket is
/// counted on its own, by asking which stored ranges overlap it, rather than
/// by reading a column of values the field does not have.
fn run_range_field_histogram(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("histogram").cloned().unwrap_or_else(|| json!({}));
    let Some(field) = spec.get("field").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return Ok(json!({"buckets": []}));
    };
    let interval = spec.get("interval").and_then(|v| v.as_f64()).filter(|i| *i > 0.0);
    let Some(interval) = interval else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "[interval] must be >0 for histogram aggregation",
        ));
    };
    let min_doc_count = spec.get("min_doc_count").and_then(|v| v.as_u64()).unwrap_or(0);
    let base = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
    let bounds = spec.get("hard_bounds").or_else(|| spec.get("extended_bounds"));
    let bound = |k: &str| bounds.and_then(|b| b.get(k)).and_then(|v| v.as_f64());

    // without bounds the span is the widest the stored endpoints reach
    let (lo, hi) = match (bound("min"), bound("max")) {
        (Some(a), Some(b)) => (a, b),
        (a, b) => {
            let probe = json!({
                "__min": {"min": {"field": format!("{field}.gte")}},
                "__max": {"max": {"field": format!("{field}.lte")}},
            });
            let (_, extremes) = filtered_count(store, targets, &base, &Some(probe))?;
            let read = |k: &str| -> Option<f64> {
                extremes.as_ref()?.get(k)?.get("value")?.as_f64()
            };
            match (a.or_else(|| read("__min")), b.or_else(|| read("__max"))) {
                (Some(x), Some(y)) => (x, y),
                _ => return Ok(json!({"buckets": []})),
            }
        }
    };
    if !lo.is_finite() || !hi.is_finite() || hi < lo {
        return Ok(json!({"buckets": []}));
    }
    // buckets start on multiples of the interval, as they do for a plain field
    let first = (lo / interval).floor() * interval;
    let steps = (((hi - first) / interval).floor() as i64).clamp(0, 65_536);
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();

    let mut buckets = Vec::new();
    for i in 0..=steps {
        let key = first + i as f64 * interval;
        // a stored range overlaps this bucket when it starts before the
        // bucket ends and ends at or after the bucket starts
        let overlap = json!({"bool": {"filter": [
            {"range": {format!("{field}.gte"): {"lt": key + interval}}},
            {"range": {format!("{field}.lte"): {"gte": key}}},
            base.clone(),
        ]}});
        let (count, sub) = count_with_sub_aggs(store, targets, &overlap, &sub_aggs, false)?;
        if count < min_doc_count {
            continue;
        }
        let mut b = json!({
            "key": if key.fract() == 0.0 { json!(key as i64) } else { json!(key) },
            "doc_count": count,
        });
        if let (Some(sub), Some(o)) = (sub, b.as_object_mut()) {
            if let Some(entries) = sub.as_object() {
                for (k, v) in entries {
                    o.insert(k.clone(), v.clone());
                }
            }
        }
        buckets.push(b);
    }
    Ok(json!({"buckets": buckets}))
}

fn run_ip_range_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("ip_range").cloned().unwrap_or(json!({}));
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let keyed = spec.get("keyed").and_then(|v| v.as_bool()).unwrap_or(false);

    let mut buckets = Vec::new();
    let mut keyed_out = serde_json::Map::new();
    for range in spec.get("ranges").and_then(|r| r.as_array()).into_iter().flatten() {
        // a mask names the same span as the addresses at its edges
        let (from, to) = match range.get("mask").and_then(|m| m.as_str()) {
            Some(mask) => match crate::store::cidr_bounds(mask) {
                Some((lo, hi)) => (Some(json!(lo)), Some(json!(hi))),
                None => (None, None),
            },
            None => (range.get("from").cloned(), range.get("to").cloned()),
        };
        let mut clause = serde_json::Map::new();
        if let Some(f) = from.as_ref().filter(|v| !v.is_null()) {
            clause.insert("gte".into(), f.clone());
        }
        if let Some(t) = to.as_ref().filter(|v| !v.is_null()) {
            clause.insert("lt".into(), t.clone());
        }
        let filter = if clause.is_empty() {
            json!({"exists": {"field": field}})
        } else {
            json!({"range": {field.clone(): Value::Object(clause)}})
        };
        let combined = combine(main_query, Some(filter));
        let (count, sub) = count_with_sub_aggs(store, targets, &combined, &sub_aggs, false)?;

        let text = |v: &Option<Value>| match v {
            Some(Value::String(s)) => s.clone(),
            Some(other) if !other.is_null() => other.to_string(),
            _ => "*".to_string(),
        };
        let key = range
            .get("key")
            .and_then(|k| k.as_str())
            .map(|s| s.to_string())
            .or_else(|| range.get("mask").and_then(|m| m.as_str()).map(|s| s.to_string()))
            .unwrap_or_else(|| format!("{}-{}", text(&from), text(&to)));
        let mut b = json!({"key": key.clone(), "doc_count": count});
        if let Some(f) = from.as_ref().filter(|v| !v.is_null()) {
            b["from"] = f.clone();
        }
        if let Some(t) = to.as_ref().filter(|v| !v.is_null()) {
            b["to"] = t.clone();
        }
        if let Some(Value::Object(o)) = sub {
            for (k, v) in o {
                b[k] = v;
            }
        }
        if keyed {
            keyed_out.insert(key, b);
        } else {
            buckets.push(b);
        }
    }
    if keyed {
        return Ok(json!({"buckets": Value::Object(keyed_out)}));
    }
    Ok(json!({"buckets": buckets}))
}

/// `adjacency_matrix`: how the named filters overlap.
///
/// One bucket per filter, and one per pair of filters for the documents both
/// select. Pairs that select nothing are left out.
fn run_adjacency_matrix_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("adjacency_matrix").cloned().unwrap_or(json!({}));
    let separator = spec
        .get("separator")
        .and_then(|v| v.as_str())
        .unwrap_or("&")
        .to_string();
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let Some(filters) = spec.get("filters").and_then(|f| f.as_object()) else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "[filters] cannot be empty",
        ));
    };
    let named: Vec<(String, Value)> =
        filters.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

    let mut buckets = Vec::new();
    let mut push = |key: String, filter: Value, buckets: &mut Vec<Value>| -> std::result::Result<(), Response> {
        let combined = combine(main_query, Some(filter));
        let (count, sub) = count_with_sub_aggs(store, targets, &combined, &sub_aggs, false)?;
        if count == 0 {
            return Ok(());
        }
        let mut b = json!({"key": key, "doc_count": count});
        if let Some(Value::Object(o)) = sub {
            for (k, v) in o {
                b[k] = v;
            }
        }
        buckets.push(b);
        Ok(())
    };
    for (name, filter) in &named {
        push(name.clone(), filter.clone(), &mut buckets)?;
    }
    for i in 0..named.len() {
        for j in (i + 1)..named.len() {
            let (a, b) = (&named[i], &named[j]);
            let both = json!({"bool": {"filter": [a.1.clone(), b.1.clone()]}});
            push(format!("{}{separator}{}", a.0, b.0), both, &mut buckets)?;
        }
    }
    buckets.sort_by(|a, b| {
        let k = |v: &Value| v.get("key").and_then(|s| s.as_str()).unwrap_or("").to_string();
        k(a).cmp(&k(b))
    });
    Ok(json!({"buckets": buckets}))
}

/// `rare_terms`: the terms few documents carry.
///
/// It is `terms` read from the other end -- keep the buckets at or below
/// `max_doc_count` instead of the largest ones -- so it is answered by
/// collecting the buckets and filtering, ordered by key.
fn run_rare_terms_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
    weighted: bool,
    agg_name: &str,
) -> std::result::Result<Value, Response> {
    let spec = def.get("rare_terms").cloned().unwrap_or(json!({}));
    let max_doc_count = spec.get("max_doc_count").and_then(|v| v.as_u64()).unwrap_or(1);
    let Some(field) = spec.get("field").and_then(|f| f.as_str()).map(|s| s.to_string()) else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "Required one of fields [field, script], but none were specified.",
        ));
    };
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();

    let ty = targets
        .iter()
        .filter_map(|n| store.get(n))
        .next()
        .and_then(|st| st.read().mapping.type_of(&field).map(|t| t.to_string()));

    // a pattern only makes sense against text; a numeric, date or address
    // field has no spelling for a regular expression to match
    for pass in ["include", "exclude"] {
        let is_pattern = matches!(spec.get(pass), Some(Value::String(_)));
        if is_pattern && !matches!(ty.as_deref(), None | Some("keyword" | "text" | "wildcard")) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!(
                    "Aggregation [{agg_name}] cannot support regular expression style \
                     include/exclude settings as they can only be applied to string fields. \
                     Use an array of values for include/exclude clauses"
                ),
            ));
        }
    }

    let mut terms = json!({"field": field, "size": 65_536});
    if let Some(v) = spec.get("missing") {
        terms["missing"] = v.clone();
    }
    // include and exclude are matched here rather than pushed down: they are
    // applied to the term dictionary, which a date or a number does not live
    // in, so pushing them down silently matches nothing
    let listed = |key: &str| -> Option<Vec<String>> {
        let v = spec.get(key)?;
        let items: Vec<String> = match v {
            Value::Array(a) => a.iter().filter_map(term_filter_text).collect(),
            other => term_filter_text(other).into_iter().collect(),
        };
        Some(items)
    };
    let include = listed("include");
    let exclude = listed("exclude");
    let mut node = json!({"terms": terms});
    if let Some(sa) = sub_aggs {
        node["aggs"] = sa;
    }
    let mut request = json!({"__rare": node});
    if weighted {
        inject_doc_count_helpers(&mut request);
    }
    let query = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
    let (_, res) = filtered_count(store, targets, &query, &Some(request))?;
    let Some(mut res) = res else { return Ok(json!({"buckets": []})) };
    if weighted {
        apply_doc_counts(&mut res);
    }
    let mut buckets: Vec<Value> = res
        .pointer("/__rare/buckets")
        .and_then(|b| b.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|b| b.get("doc_count").and_then(|c| c.as_u64()).unwrap_or(0) <= max_doc_count)
        .filter(|b| {
            let key = b.get("key").cloned().unwrap_or(Value::Null);
            let shown = terms_key_view(key, ty.as_deref());
            let matches = |list: &Vec<String>| {
                list.iter().any(|want| term_filter_matches(want, &shown, ty.as_deref()))
            };
            include.as_ref().map(|l| matches(l)).unwrap_or(true)
                && !exclude.as_ref().map(|l| matches(l)).unwrap_or(false)
        })
        .map(|mut b| {
            if let Some(o) = b.as_object_mut() {
                if let Some(raw) = o.get("key").cloned() {
                    let (k, as_string) = terms_key_view(raw, ty.as_deref());
                    o.insert("key".into(), k);
                    match as_string {
                        Some(s) => {
                            o.insert("key_as_string".into(), Value::String(s));
                        }
                        None => {
                            o.remove("key_as_string");
                        }
                    }
                }
                o.remove(DC_SUM);
                o.remove(DC_CNT);
            }
            b
        })
        .collect();

    // rarest first, which is the whole point of the aggregation, and the key
    // settles the order among buckets that are equally rare
    buckets.sort_by(|a, b| {
        let c = |v: &Value| v.get("doc_count").and_then(|d| d.as_u64()).unwrap_or(0);
        let k = |v: &Value| v.get("key").cloned().unwrap_or(Value::Null);
        c(a).cmp(&c(b)).then_with(|| match (k(a), k(b)) {
            (Value::String(x), Value::String(y)) => x.cmp(&y),
            (Value::Number(x), Value::Number(y)) => x
                .as_f64()
                .unwrap_or(0.0)
                .partial_cmp(&y.as_f64().unwrap_or(0.0))
                .unwrap_or(Ordering::Equal),
            (x, y) => x.to_string().cmp(&y.to_string()),
        })
    });
    Ok(json!({"buckets": buckets}))
}

/// One entry of an include/exclude list, as text.
fn term_filter_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Does one include/exclude entry name this bucket?
///
/// The caller writes a date or an address the way it was sent; the bucket
/// carries the way it is read back. Both are put in one spelling before they
/// are compared.
fn term_filter_matches(want: &str, shown: &(Value, Option<String>), ty: Option<&str>) -> bool {
    let (key, as_string) = shown;
    match ty {
        Some("date") | Some("date_nanos") => {
            let a = crate::store::canonical_date(&Value::String(want.to_string()));
            let b = as_string
                .clone()
                .and_then(|s| crate::store::canonical_date(&Value::String(s)));
            a.is_some() && a == b
        }
        Some("ip") => {
            let a = crate::store::canonical_ip(want);
            let b = key.as_str().and_then(crate::store::canonical_ip);
            a.is_some() && a == b
        }
        _ => {
            let text = match key {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            text == want
        }
    }
}

/// How a term key and its readable form are written for a field of this type.
fn terms_key_view(raw: Value, ty: Option<&str>) -> (Value, Option<String>) {
    match ty {
        Some("ip") => {
            let shown = raw.as_str().and_then(crate::store::ip_from_canonical);
            (shown.map(Value::String).unwrap_or(raw), None)
        }
        Some("boolean") => {
            let n = raw.as_u64().unwrap_or(0);
            (json!(n), Some(if n != 0 { "true".into() } else { "false".into() }))
        }
        Some("date") | Some("date_nanos") => {
            let iso = raw.as_str().and_then(crate::store::canonical_date_str);
            let millis = raw
                .as_str()
                .and_then(crate::store::parse_date_lenient)
                .map(|d| d.unix_timestamp_nanos() / 1_000_000);
            match (millis, iso) {
                (Some(ms), Some(iso)) => (json!(ms as i64), Some(iso)),
                _ => (raw, None),
            }
        }
        _ => (raw, None),
    }
}

/// `multi_terms`: one bucket per combination of several fields' values.
///
/// The combinations come from nesting a `terms` aggregation per field and
/// flattening the tree, the same way `composite` builds its keys. What differs
/// is the answer: a key is the list of values rather than a named object, and
/// buckets are ranked by how many documents they hold rather than by key.
fn run_multi_terms_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
    weighted: bool,
) -> std::result::Result<Value, Response> {
    let spec = def.get("multi_terms").cloned().unwrap_or(json!({}));
    let size = spec.get("size").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
    let min_doc_count = spec.get("min_doc_count").and_then(|v| v.as_u64()).unwrap_or(1);
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();

    let mut fields: Vec<String> = Vec::new();
    let mut missings: Vec<Option<Value>> = Vec::new();
    for entry in spec.get("terms").and_then(|v| v.as_array()).into_iter().flatten() {
        missings.push(entry.get("missing").cloned());
        match entry.get("field").and_then(|f| f.as_str()) {
            Some(f) => fields.push(f.to_string()),
            None => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    "Required one of fields [field, script], but none were specified.",
                ));
            }
        }
    }
    if fields.len() < 2 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "multi term aggregation must has at least 2 terms",
        ));
    }

    // an aggregation tantivy cannot parse cannot ride down with the terms
    // request; it is run per bucket once the buckets are known
    let (peeled_subs, plain_subs) = split_peelable(&sub_aggs);
    let mut request = plain_subs.clone().unwrap_or_else(|| json!({}));
    for (i, field) in fields.iter().enumerate().rev() {
        let mut node = json!({"terms": {"field": field, "size": 65_536}});
        // a field the document has no value for still takes part when the
        // request says what to stand in
        if let Some(m) = missings.get(i).and_then(|m| m.clone()) {
            node["terms"]["missing"] = m;
        }
        if request.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
            node["aggs"] = request;
        }
        request = json!({format!("__m{i}"): node});
    }
    if weighted {
        inject_doc_count_helpers(&mut request);
    }
    let query = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
    let (_, res) = filtered_count(store, targets, &query, &Some(request))?;
    let Some(mut res) = res else { return Ok(json!({"buckets": []})) };
    if weighted {
        apply_doc_counts(&mut res);
    }

    // the mapped type decides how a key is written back out
    let types: Vec<Option<String>> = targets
        .iter()
        .filter_map(|n| store.get(n))
        .next()
        .map(|st| {
            let g = st.read();
            fields.iter().map(|f| g.mapping.type_of(f).map(|t| t.to_string())).collect()
        })
        .unwrap_or_else(|| fields.iter().map(|_| None).collect());

    let mut flat: Vec<(Vec<Value>, u64, serde_json::Map<String, Value>)> = Vec::new();
    flatten_multi_terms(&res, 0, fields.len(), &mut Vec::new(), &mut flat);

    let total: u64 = flat.iter().map(|(_, c, _)| *c).sum();
    let mut buckets: Vec<Value> = flat
        .into_iter()
        .filter(|(_, c, _)| *c >= min_doc_count)
        .map(|(key, count, subs)| {
            let key: Vec<Value> = key
                .into_iter()
                .zip(types.iter())
                .map(|(v, t)| multi_terms_key(v, t.as_deref()))
                .collect();
            let as_string = key
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join("|");
            let mut b = json!({
                "key": key,
                "key_as_string": as_string,
                "doc_count": count,
            });
            for (k, v) in subs {
                b[k] = v;
            }
            b
        })
        .collect();

    // `order` names sub-aggregations or the bucket itself, and may name
    // several in turn; without it, most documents first
    let orders = parse_multi_terms_order(spec.get("order"));
    buckets.sort_by(|a, b| {
        for (key, desc) in &orders {
            let ord = compare_bucket_by(a, b, key);
            let ord = if *desc { ord.reverse() } else { ord };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        // a settled order among equals
        let k = |v: &Value| {
            v.get("key_as_string").and_then(|s| s.as_str()).unwrap_or("").to_string()
        };
        k(a).cmp(&k(b))
    });
    let kept: u64 = buckets.iter().take(size).map(|b| {
        b.get("doc_count").and_then(|d| d.as_u64()).unwrap_or(0)
    }).sum();
    buckets.truncate(size);
    // now that the buckets are settled, each one narrows the query for the
    // aggregations that had to be left behind
    if let Some(peeled) = peeled_subs.as_ref().and_then(|v| v.as_object()) {
        for b in buckets.iter_mut() {
            let keys = b.get("key").and_then(|k| k.as_array()).cloned().unwrap_or_default();
            let mut filters: Vec<Value> = fields
                .iter()
                .zip(keys.iter())
                .map(|(f, k)| json!({"term": {f.clone(): k.clone()}}))
                .collect();
            if let Some(q) = main_query.as_ref() {
                filters.push(q.clone());
            }
            let narrowed = Some(json!({"bool": {"filter": filters}}));
            for (n, d) in peeled {
                b[n.clone()] = run_peeled_agg(store, targets, &narrowed, n, d, weighted)?;
            }
        }
    }
    Ok(json!({
        "doc_count_error_upper_bound": 0,
        "sum_other_doc_count": total.saturating_sub(kept),
        "buckets": buckets,
    }))
}

/// Read the `order` clause: one object, or several applied in turn.
fn parse_multi_terms_order(order: Option<&Value>) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut take = |v: &Value| {
        if let Some(o) = v.as_object() {
            for (k, dir) in o {
                let desc = dir.as_str().map(|d| d == "desc").unwrap_or(false);
                out.push((k.clone(), desc));
            }
        }
    };
    match order {
        Some(Value::Array(items)) => items.iter().for_each(&mut take),
        Some(v) => take(v),
        None => out.push(("_count".to_string(), true)),
    }
    if out.is_empty() {
        out.push(("_count".to_string(), true));
    }
    out
}

/// Compare two buckets by one ordering key: the document count, the key
/// itself, or the value of a sub-aggregation.
fn compare_bucket_by(a: &Value, b: &Value, key: &str) -> Ordering {
    match key {
        "_count" => {
            let c = |v: &Value| v.get("doc_count").and_then(|d| d.as_u64()).unwrap_or(0);
            c(a).cmp(&c(b))
        }
        "_key" | "_term" => {
            let k = |v: &Value| {
                v.get("key_as_string").and_then(|s| s.as_str()).unwrap_or("").to_string()
            };
            k(a).cmp(&k(b))
        }
        metric => {
            let m = |v: &Value| {
                v.pointer(&format!("/{metric}/value")).and_then(|x| x.as_f64()).unwrap_or(f64::MIN)
            };
            m(a).partial_cmp(&m(b)).unwrap_or(Ordering::Equal)
        }
    }
}

/// A key element in the spelling its field's type is read in.
fn multi_terms_key(v: Value, ty: Option<&str>) -> Value {
    match ty {
        Some("ip") => v
            .as_str()
            .and_then(crate::store::ip_from_canonical)
            .map(Value::String)
            .unwrap_or(v),
        Some("boolean") => match v.as_u64() {
            Some(n) => Value::Bool(n != 0),
            None => v,
        },
        Some("date") | Some("date_nanos") => v
            .as_str()
            .and_then(crate::store::canonical_date_str)
            .map(Value::String)
            .unwrap_or(v),
        _ => v,
    }
}

fn flatten_multi_terms(
    node: &Value,
    depth: usize,
    total_depth: usize,
    key: &mut Vec<Value>,
    out: &mut Vec<(Vec<Value>, u64, serde_json::Map<String, Value>)>,
) {
    let Some(buckets) = node.pointer(&format!("/__m{depth}/buckets")).and_then(|b| b.as_array())
    else {
        return;
    };
    for b in buckets {
        key.push(b.get("key").cloned().unwrap_or(Value::Null));
        if depth + 1 < total_depth {
            flatten_multi_terms(b, depth + 1, total_depth, key, out);
        } else {
            let count = b.get("doc_count").and_then(|c| c.as_u64()).unwrap_or(0);
            let mut subs = serde_json::Map::new();
            if let Some(o) = b.as_object() {
                for (k, v) in o {
                    if k != "key" && k != "doc_count" && k != "key_as_string" && !k.starts_with("__m")
                    {
                        subs.insert(k.clone(), v.clone());
                    }
                }
            }
            out.push((key.clone(), count, subs));
        }
        key.pop();
    }
}

/// A composite aggregation over `terms` sources.
///
/// The sources are run as nested `terms` aggregations and the resulting tree is
/// flattened into one bucket per combination, which is what a composite is. Key
/// order is ascending across the whole tuple, as the paging contract requires.

/// `weighted_avg`: sum(value * weight) / sum(weight), paired per document.
fn run_weighted_avg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("weighted_avg").cloned().unwrap_or(json!({}));
    let read = |key: &str| -> (String, Option<f64>) {
        let side = spec.get(key).cloned().unwrap_or(json!({}));
        agg_field_and_missing(&side)
    };
    let (vf, vmiss) = read("value");
    let (wf, wmiss) = read("weight");
    let query = combine(main_query, None);
    let pairs = collect_field_pairs(store, targets, &query, &vf, vmiss, &wf, wmiss)?;

    let mut num = 0.0f64;
    let mut den = 0.0f64;
    for (v, w) in pairs {
        num += v * w;
        den += w;
    }
    if den == 0.0 {
        return Ok(json!({"value": Value::Null}));
    }
    Ok(json!({"value": num / den}))
}

/// Read two columns side by side, one pair per document that has both.
fn collect_field_pairs(
    store: &Store,
    targets: &[String],
    query_json: &Value,
    a_field: &str,
    a_missing: Option<f64>,
    b_field: &str,
    b_missing: Option<f64>,
) -> std::result::Result<Vec<(f64, f64)>, Response> {
    let mut out = Vec::new();
    for name in targets {
        let Some(st) = store.get(name) else { continue };
        let g = st.read();
        let ctx = Ctx {
            fields: &g.fields,
            mapping: &g.mapping,
            index: &g.index,
            max_terms_count: g.max_terms_count(),
            observed_kinds: &g.observed_kinds,
            kinds_complete: g.kinds_complete,
            stats: &g.stats,
        };
        let q = crate::query::build(&ctx, query_json)
            .map_err(|e| err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string()))?;
        let (a_col, b_col) = (ctx.column_name(a_field, false), ctx.column_name(b_field, false));
        let searcher = g.reader.searcher();
        let addrs = searcher
            .search(&q, &tantivy::collector::DocSetCollector)
            .map_err(|e| {
                err(StatusCode::BAD_REQUEST, "search_phase_execution_exception", e.to_string())
            })?;
        let cols: Vec<(SortColumns, SortColumns)> = searcher
            .segment_readers()
            .iter()
            .map(|r| (SortColumns::for_segment(r, &a_col), SortColumns::for_segment(r, &b_col)))
            .collect();
        for addr in addrs {
            let Some((ca, cb)) = cols.get(addr.segment_ord as usize) else { continue };
            let av = ca.numeric_values(addr.doc_id);
            let bv = cb.numeric_values(addr.doc_id);
            let a = av.first().copied().or(a_missing);
            let b = bv.first().copied().or(b_missing);
            if let (Some(a), Some(b)) = (a, b) {
                out.push((a, b));
            }
        }
    }
    Ok(out)
}

/// The sibling pipelines: aggregations whose input is other aggregations'
/// buckets rather than documents.
const PIPELINES: &[&str] =
    &["avg_bucket", "sum_bucket", "min_bucket", "max_bucket", "stats_bucket"];

fn is_pipeline_agg(def: &Value) -> bool {
    def.as_object()
        .map(|o| o.keys().any(|k| PIPELINES.contains(&k.as_str())))
        .unwrap_or(false)
}

fn run_pipeline_agg(aggs: &Value, def: &Value) -> std::result::Result<Value, Response> {
    let Some(o) = def.as_object() else { return Ok(Value::Null) };
    let mut kind = String::new();
    for k in o.keys() {
        if PIPELINES.contains(&k.as_str()) {
            kind = k.clone();
            break;
        }
    }
    if kind.is_empty() {
        return Ok(Value::Null);
    }
    let spec = o.get(&kind).cloned().unwrap_or(Value::Null);
    let path = spec.get("buckets_path").and_then(|v| v.as_str()).unwrap_or("");
    let values = resolve_buckets_path(aggs, path);
    if values.is_empty() {
        return Ok(json!({"value": Value::Null}));
    }
    let sum: f64 = values.iter().sum();
    let n = values.len() as f64;
    let value = match kind.as_str() {
        "avg_bucket" => sum / n,
        "sum_bucket" => sum,
        "min_bucket" => values.iter().copied().fold(f64::INFINITY, f64::min),
        "max_bucket" => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        "stats_bucket" => {
            return Ok(json!({
                "count": values.len(),
                "min": values.iter().copied().fold(f64::INFINITY, f64::min),
                "max": values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                "avg": sum / n,
                "sum": sum,
            }));
        }
        _ => return Ok(json!({"value": Value::Null})),
    };
    Ok(json!({"value": value}))
}

/// `histo.v` means: the metric `v` of every bucket of `histo`.
fn resolve_buckets_path(aggs: &Value, path: &str) -> Vec<f64> {
    let mut segs = path.split('>').flat_map(|s| s.split('.'));
    let Some(first) = segs.next() else { return Vec::new() };
    let rest: Vec<&str> = segs.collect();
    let Some(node) = aggs.get(first) else { return Vec::new() };
    let Some(buckets) = node.get("buckets").and_then(|b| b.as_array()) else {
        return Vec::new();
    };
    buckets
        .iter()
        // an empty bucket has no value to contribute, which is what the
        // default gap policy asks for
        .filter(|b| b.get("doc_count").and_then(|c| c.as_u64()).map(|c| c > 0).unwrap_or(true))
        .filter_map(|b| {
            let mut cur = b;
            for seg in &rest {
                cur = cur.get(seg)?;
            }
            cur.get("value").and_then(|v| v.as_f64()).or_else(|| cur.as_f64())
        })
        .collect()
}


/// `variable_width_histogram`: buckets whose edges follow the data.
///
/// The values are sorted and cut at the widest gaps, which puts the boundaries
/// where the data is already sparse. Each bucket is keyed by the mean of what
/// it holds.
fn run_variable_width_histogram(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("variable_width_histogram").cloned().unwrap_or(json!({}));
    let want = spec.get("buckets").and_then(|v| v.as_u64()).unwrap_or(10).max(1) as usize;
    let (field, missing) = agg_field_and_missing(&spec);
    let query = combine(main_query, None);
    let mut values = collect_field_values(store, targets, &query, &field, missing)?;
    if values.is_empty() {
        return Ok(json!({"buckets": []}));
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));

    // cut where the data is sparsest: the widest gaps between neighbours
    let mut gaps: Vec<(f64, usize)> =
        (1..values.len()).map(|i| (values[i] - values[i - 1], i)).collect();
    gaps.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
    let mut cuts: Vec<usize> = gaps.into_iter().take(want.saturating_sub(1)).map(|(_, i)| i).collect();
    cuts.sort_unstable();

    let mut buckets = Vec::new();
    let mut start = 0usize;
    for end in cuts.into_iter().chain(std::iter::once(values.len())) {
        let slice = &values[start..end];
        if slice.is_empty() {
            continue;
        }
        let sum: f64 = slice.iter().sum();
        buckets.push(json!({
            "min": slice[0],
            "key": sum / slice.len() as f64,
            "max": slice[slice.len() - 1],
            "doc_count": slice.len(),
        }));
        start = end;
    }
    Ok(json!({"buckets": buckets}))
}

/// `auto_date_histogram`: pick the smallest rounding that keeps the bucket
/// count within the target, then bucket by it.
///
/// The choice is made from the span the data actually covers rather than by
/// building each candidate histogram: at one-second resolution a week-long
/// span is over half a million buckets, which is a lot of searching to do only
/// to discard it.
fn run_auto_date_histogram(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("auto_date_histogram").cloned().unwrap_or(json!({}));
    let want = spec.get("buckets").and_then(|v| v.as_u64()).unwrap_or(10).max(1);
    let field = spec.get("field").cloned().unwrap_or(Value::Null);
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();

    let base = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
    let probe = json!({
        "__min": {"min": {"field": field}},
        "__max": {"max": {"field": field}},
    });
    let (_, extremes) = filtered_count(store, targets, &base, &Some(probe))?;
    let read = |k: &str| -> Option<f64> { extremes.as_ref()?.get(k)?.get("value")?.as_f64() };
    let (Some(lo), Some(hi)) = (read("__min"), read("__max")) else {
        return Ok(json!({"buckets": [], "interval": "1s"}));
    };
    let span_ns = (hi - lo).max(0.0);

    // label, the unit the histogram steps by, and roughly how long it is
    const NS: f64 = 1e9;
    const STEPS: &[(&str, &str, f64)] = &[
        ("1s", "second", NS),
        ("1m", "minute", 60.0 * NS),
        ("1h", "hour", 3600.0 * NS),
        ("1d", "day", 86_400.0 * NS),
        ("7d", "week_sunday", 604_800.0 * NS),
        ("1M", "month", 2_629_746.0 * NS),
        ("3M", "quarter", 7_889_238.0 * NS),
        ("1y", "year", 31_556_952.0 * NS),
    ];
    let (label, unit) = STEPS
        .iter()
        .find(|(_, _, len)| (span_ns / len).floor() + 1.0 <= want as f64)
        .map(|(l, u, _)| (*l, *u))
        .unwrap_or(("1y", "year"));

    let mut request = json!({
        "date_histogram": {
            "field": field,
            "calendar_interval": unit,
            "min_doc_count": 1,
        },
    });
    if let Some(sa) = sub_aggs {
        request["aggs"] = sa;
    }
    let mut out = run_calendar_histogram(store, targets, main_query, &request)?;
    out["interval"] = json!(label);
    Ok(out)
}

fn run_composite_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
    weighted: bool,
) -> std::result::Result<Value, Response> {
    let spec = def.get("composite").cloned().unwrap_or(json!({}));
    let size = spec.get("size").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
    let after = spec.get("after").cloned();
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();

    // `sources` is a list of single-key objects, each naming one source
    let mut sources: Vec<(String, String)> = Vec::new();
    for entry in spec.get("sources").and_then(|v| v.as_array()).into_iter().flatten() {
        let Some((name, body)) = entry.as_object().and_then(|o| o.iter().next()) else {
            continue;
        };
        let Some(field) = body.pointer("/terms/field").and_then(|f| f.as_str()) else {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                "[composite] only supports `terms` sources",
            ));
        };
        sources.push((name.clone(), field.to_string()));
    }
    if sources.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "[composite] requires at least one source",
        ));
    }

    // nest the sources outermost-first; the innermost carries the sub-aggs
    let mut request = sub_aggs.clone().unwrap_or_else(|| json!({}));
    for (i, (_, field)) in sources.iter().enumerate().rev() {
        let mut node = json!({
            "terms": {"field": field, "size": 65_536, "order": {"_key": "asc"}}
        });
        if request.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
            node["aggs"] = request;
        }
        request = json!({format!("__c{i}"): node});
    }
    if weighted {
        inject_doc_count_helpers(&mut request);
    }

    let query = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
    let (_, res) = filtered_count(store, targets, &query, &Some(request))?;
    let Some(mut res) = res else { return Ok(json!({"buckets": []})) };
    if weighted {
        apply_doc_counts(&mut res);
    }

    let mut flat: Vec<Value> = Vec::new();
    flatten_composite(&res, 0, &sources, &mut serde_json::Map::new(), &mut flat);
    flat.sort_by(|a, b| composite_key_order(a, b, &sources));

    if let Some(after) = after.as_ref().and_then(|a| a.as_object()) {
        let marker = json!({"key": Value::Object(after.clone())});
        flat.retain(|b| composite_key_order(b, &marker, &sources) == Ordering::Greater);
    }
    let more = flat.len() > size;
    flat.truncate(size);
    let mut out = json!({"buckets": flat});
    if more || after.is_some() {
        if let Some(last) = out["buckets"].as_array().and_then(|a| a.last()) {
            out["after_key"] = last["key"].clone();
        }
    }
    Ok(out)
}

fn flatten_composite(
    node: &Value,
    depth: usize,
    sources: &[(String, String)],
    key: &mut serde_json::Map<String, Value>,
    out: &mut Vec<Value>,
) {
    let Some(buckets) = node.pointer(&format!("/__c{depth}/buckets")).and_then(|b| b.as_array())
    else {
        return;
    };
    for b in buckets {
        key.insert(sources[depth].0.clone(), b.get("key").cloned().unwrap_or(Value::Null));
        if depth + 1 < sources.len() {
            flatten_composite(b, depth + 1, sources, key, out);
        } else {
            let mut bucket = json!({
                "key": Value::Object(key.clone()),
                "doc_count": b.get("doc_count").cloned().unwrap_or(json!(0)),
            });
            // anything else under the bucket is a sub-aggregation of the composite
            if let Some(o) = b.as_object() {
                for (k, v) in o {
                    if k != "key" && k != "doc_count" && !k.starts_with("__c") {
                        bucket[k] = v.clone();
                    }
                }
            }
            out.push(bucket);
        }
    }
    key.remove(&sources[depth].0);
}

fn composite_key_order(a: &Value, b: &Value, sources: &[(String, String)]) -> Ordering {
    for (name, _) in sources {
        let (x, y) = (a.pointer(&format!("/key/{name}")), b.pointer(&format!("/key/{name}")));
        let ord = match (x, y) {
            (Some(Value::Number(m)), Some(Value::Number(n))) => m
                .as_f64()
                .unwrap_or(0.0)
                .partial_cmp(&n.as_f64().unwrap_or(0.0))
                .unwrap_or(Ordering::Equal),
            (Some(Value::String(m)), Some(Value::String(n))) => m.cmp(n),
            (Some(m), Some(n)) => m.to_string().cmp(&n.to_string()),
            _ => Ordering::Equal,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

fn run_calendar_histogram(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    use tantivy::time::{Duration, OffsetDateTime};

    let spec = def.get("date_histogram").cloned().unwrap_or(json!({}));
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    let interval = spec
        .get("calendar_interval")
        .and_then(|v| v.as_str())
        .unwrap_or("day")
        .to_string();
    let Some(unit) = CalendarUnit::parse(&interval) else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("The supplied interval [{interval}] could not be parsed as a calendar interval."),
        ));
    };
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let min_doc_count =
        spec.get("min_doc_count").and_then(|v| v.as_u64()).unwrap_or(0);

    // the span to cover comes from the extremes the query actually matches
    // a range-typed field has no single value per document, so it has no
    // extremes to read; its span has to come from the bounds the request gives
    let ranged = targets
        .iter()
        .filter_map(|n| store.get(n))
        .any(|st| {
            st.read()
                .mapping
                .type_of(&field)
                .map(|t| t.ends_with("_range"))
                .unwrap_or(false)
        });
    let bounds = spec.get("hard_bounds").or_else(|| spec.get("extended_bounds"));
    let (mut lo_ns, mut hi_ns) = (0.0f64, 0.0f64);
    if !ranged {
        let base = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
        let probe = json!({
            "__min": {"min": {"field": field}},
            "__max": {"max": {"field": field}},
        });
        let (_, extremes) = filtered_count(store, targets, &base, &Some(probe))?;
        // the date column counts in nanoseconds, which is what min/max read out
        let read = |k: &str| -> Option<f64> {
            extremes.as_ref()?.get(k)?.get("value")?.as_f64()
        };
        let (Some(a), Some(b)) = (read("__min"), read("__max")) else {
            return Ok(json!({"buckets": []}));
        };
        (lo_ns, hi_ns) = (a, b);
    } else if bounds.is_none() {
        // a range field has no single value, but the endpoints it stores do:
        // the span runs from the earliest start to the latest end
        let base = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
        let probe = json!({
            "__min": {"min": {"field": format!("{field}.gte")}},
            "__max": {"max": {"field": format!("{field}.lte")}},
        });
        let (_, extremes) = filtered_count(store, targets, &base, &Some(probe))?;
        let read = |k: &str| -> Option<f64> {
            extremes.as_ref()?.get(k)?.get("value")?.as_f64()
        };
        match (read("__min"), read("__max")) {
            (Some(a), Some(b)) => (lo_ns, hi_ns) = (a, b),
            _ => return Ok(json!({"buckets": []})),
        }
    }
    // bounds are written the way a document would be, so they arrive in
    // milliseconds and have to meet the column's nanoseconds
    let bound_ns = |key: &str| -> Option<f64> {
        let v = bounds?.get(key)?;
        crate::store::canonical_date(v)
            .and_then(|d| crate::store::parse_date_lenient(&d))
            .map(|d| d.unix_timestamp_nanos() as f64)
    };
    let lo_ns = bound_ns("min").unwrap_or(lo_ns);
    let hi_ns = bound_ns("max").unwrap_or(hi_ns);

    let to_dt = |ns: f64| -> Option<OffsetDateTime> {
        OffsetDateTime::from_unix_timestamp_nanos(ns as i128).ok()
    };
    let (Some(lo), Some(hi)) = (to_dt(lo_ns), to_dt(hi_ns)) else {
        return Ok(json!({"buckets": []}));
    };

    // `offset` shifts the whole grid of boundaries. The buckets keep their
    // calendar width; they just no longer start on the calendar unit.
    let offset = spec
        .get("offset")
        .and_then(|v| v.as_str())
        .and_then(parse_offset)
        .unwrap_or(Duration::seconds(0));
    let shift = |dt: OffsetDateTime| unit.floor(dt - offset) + offset;

    let mut buckets = Vec::new();
    let mut cursor = shift(lo);
    let last = shift(hi);
    // a runaway interval would otherwise spin: no calendar histogram the suite
    // or a sane request produces comes near this
    let mut guard = 0;
    while cursor <= last && guard < 100_000 {
        guard += 1;
        let next = unit.advance(cursor - offset) + offset;
        let mut spec = json!({
            "gte": iso_millis(cursor),
            "lt": iso_millis(next),
            "format": "strict_date_optional_time",
        });
        if ranged {
            // a stored interval belongs to every bucket it touches
            spec["relation"] = json!("intersects");
        }
        let range = json!({"range": {field.clone(): spec}});
        let combined = combine(main_query, Some(range));
        let (count, sub) = count_with_sub_aggs(store, targets, &combined, &sub_aggs, false)?;
        if count >= min_doc_count {
            let mut b = json!({
                "key": cursor.unix_timestamp_nanos() as i64 / 1_000_000,
                "key_as_string": iso_millis(cursor),
                "doc_count": count,
            });
            if let Some(Value::Object(o)) = sub {
                for (k, v) in o {
                    b[k] = v;
                }
            }
            buckets.push(b);
        }
        if next == cursor {
            break;
        }
        cursor = next;
    }
    // buckets are built in calendar order; `order` may want another. `_time`
    // is the old spelling of `_key`, and both name the bucket's own date.
    if let Some((key, desc)) = spec
        .get("order")
        .and_then(|o| o.as_object())
        .and_then(|o| o.iter().next())
        .map(|(k, v)| (k.clone(), v.as_str() == Some("desc")))
    {
        let by = |f: fn(&Value) -> i64| {
            move |a: &Value, b: &Value| if desc { f(b).cmp(&f(a)) } else { f(a).cmp(&f(b)) }
        };
        match key.as_str() {
            "_key" | "_time" => {
                buckets.sort_by(by(|x| x.get("key").and_then(|v| v.as_i64()).unwrap_or(0)))
            }
            "_count" => buckets
                .sort_by(by(|x| x.get("doc_count").and_then(|v| v.as_i64()).unwrap_or(0))),
            _ => {}
        }
    }
    let _ = Duration::seconds(0);
    Ok(json!({"buckets": buckets}))
}

/// `offset` as written on a date histogram: a signed count of fixed time
/// units. Calendar units are not allowed here -- only lengths that are the
/// same wherever on the calendar they land.
fn parse_offset(s: &str) -> Option<tantivy::time::Duration> {
    let s = s.trim();
    let (sign, rest) = match s.strip_prefix('-') {
        Some(r) => (-1, r),
        None => (1, s.strip_prefix('+').unwrap_or(s)),
    };
    let split = rest.find(|c: char| !c.is_ascii_digit())?;
    let (n, unit) = rest.split_at(split);
    let n: i64 = n.parse().ok()?;
    let n = n * sign;
    Some(match unit {
        "ms" => tantivy::time::Duration::milliseconds(n),
        "s" => tantivy::time::Duration::seconds(n),
        "m" => tantivy::time::Duration::minutes(n),
        "h" | "H" => tantivy::time::Duration::hours(n),
        "d" => tantivy::time::Duration::days(n),
        _ => return None,
    })
}

fn iso_millis(dt: tantivy::time::OffsetDateTime) -> String {
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

#[derive(Clone, Copy)]
enum CalendarUnit {
    Second,
    Minute,
    Hour,
    Day,
    Week,
    /// auto_date_histogram's seven-day rounding starts its weeks on Sunday,
    /// unlike `calendar_interval: week`
    WeekSunday,
    Month,
    Quarter,
    Year,
}

impl CalendarUnit {
    fn parse(s: &str) -> Option<CalendarUnit> {
        Some(match s {
            "second" | "1s" => CalendarUnit::Second,
            "minute" | "1m" => CalendarUnit::Minute,
            "hour" | "1h" => CalendarUnit::Hour,
            "day" | "1d" => CalendarUnit::Day,
            "week" | "1w" => CalendarUnit::Week,
            "week_sunday" => CalendarUnit::WeekSunday,
            "month" | "1M" => CalendarUnit::Month,
            "quarter" | "1q" => CalendarUnit::Quarter,
            "year" | "1y" => CalendarUnit::Year,
            _ => return None,
        })
    }

    fn floor(self, dt: tantivy::time::OffsetDateTime) -> tantivy::time::OffsetDateTime {
        use tantivy::time::{Date, Month, Time};
        let midnight = |d: Date| d.with_time(Time::MIDNIGHT).assume_utc();
        match self {
            CalendarUnit::Second => dt.replace_nanosecond(0).unwrap(),
            CalendarUnit::Minute => dt.replace_second(0).unwrap().replace_nanosecond(0).unwrap(),
            CalendarUnit::Hour => dt
                .replace_minute(0)
                .unwrap()
                .replace_second(0)
                .unwrap()
                .replace_nanosecond(0)
                .unwrap(),
            CalendarUnit::Day => midnight(dt.date()),
            // calendar weeks start on Monday
            CalendarUnit::Week => {
                let back = dt.weekday().number_days_from_monday() as i64;
                midnight(dt.date() - tantivy::time::Duration::days(back))
            }
            CalendarUnit::WeekSunday => {
                let back = dt.weekday().number_days_from_sunday() as i64;
                midnight(dt.date() - tantivy::time::Duration::days(back))
            }
            CalendarUnit::Month => midnight(
                Date::from_calendar_date(dt.year(), dt.month(), 1).unwrap(),
            ),
            CalendarUnit::Quarter => {
                let m = ((dt.month() as u8 - 1) / 3) * 3 + 1;
                midnight(
                    Date::from_calendar_date(dt.year(), Month::try_from(m).unwrap(), 1).unwrap(),
                )
            }
            CalendarUnit::Year => {
                midnight(Date::from_calendar_date(dt.year(), Month::January, 1).unwrap())
            }
        }
    }

    fn advance(self, dt: tantivy::time::OffsetDateTime) -> tantivy::time::OffsetDateTime {
        use tantivy::time::{Date, Duration, Month, Time};
        let add_months = |dt: tantivy::time::OffsetDateTime, n: u32| {
            let total = dt.year() * 12 + (dt.month() as i32 - 1) + n as i32;
            let (y, m) = (total.div_euclid(12), total.rem_euclid(12) as u8 + 1);
            Date::from_calendar_date(y, Month::try_from(m).unwrap(), 1)
                .unwrap()
                .with_time(Time::MIDNIGHT)
                .assume_utc()
        };
        match self {
            CalendarUnit::Second => dt + Duration::seconds(1),
            CalendarUnit::Minute => dt + Duration::minutes(1),
            CalendarUnit::Hour => dt + Duration::hours(1),
            CalendarUnit::Day => dt + Duration::days(1),
            CalendarUnit::Week | CalendarUnit::WeekSunday => dt + Duration::days(7),
            CalendarUnit::Month => add_months(dt, 1),
            CalendarUnit::Quarter => add_months(dt, 3),
            CalendarUnit::Year => add_months(dt, 12),
        }
    }
}

fn run_mad_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("median_absolute_deviation").cloned().unwrap_or(json!({}));
    if let Some(c) = spec.get("compression").and_then(|v| v.as_f64()) {
        if c <= 0.0 {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("[compression] must be greater than 0. Found [{c:?}] in [mad]"),
            ));
        }
    }
    let (field, missing) = agg_field_and_missing(&spec);
    let query = combine(main_query, None);
    let mut values = collect_field_values(store, targets, &query, &field, missing)?;
    Ok(json!({ "value": crate::hdr::median_absolute_deviation(&mut values) }))
}
