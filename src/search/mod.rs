//! The search path: query execution, hit assembly, sorting and aggregations.
//!
//! Aggregation requests are handed to BoostCore almost untouched -- its
//! aggregation JSON already matches OpenSearch's -- after rewriting each
//! `field` onto the JSON view that backs it.

use crate::api::{Params, apply_source_selector, err, err_caused_by, no_such_index};
use crate::query::{Ctx, View};
use crate::store::{IdxState, Store};
use axum::http::StatusCode;
use axum::response::Response;
use boostcore::aggregation::AggContextParams;
use boostcore::aggregation::DistributedAggregationCollector;
use boostcore::aggregation::agg_req::Aggregations;
use boostcore::aggregation::intermediate_agg_result::IntermediateAggregationResults;
use boostcore::collector::{Count, TopDocs};
use boostcore::schema::Value as _;
use boostcore::{DocAddress, Searcher, TantivyDocument};
use serde_json::{Value, json};
use std::cmp::Ordering;

mod calendar;
pub(crate) use calendar::*;
mod candidates;
pub(crate) use candidates::*;
mod limits;
pub(crate) use limits::*;
mod shard;
pub(crate) use shard::*;

mod extras;
pub(crate) use extras::*;
mod geo;
pub(crate) use geo::*;
mod highlight;
pub(crate) use highlight::*;
mod lookup;
pub(crate) use lookup::*;
mod nested;
pub(crate) use nested::*;
mod page;
pub(crate) use page::*;
mod profile;
pub(crate) use profile::*;
mod routing;
pub(crate) use routing::*;
mod sort;
pub(crate) use sort::*;
mod suggest;
pub(crate) use suggest::*;
mod aggs;
pub(crate) use aggs::*;

const DEFAULT_TRACK_TOTAL_HITS: u64 = 10_000;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SortValue {
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
            (a, b) => a.as_f64().partial_cmp(&b.as_f64()).unwrap_or(Ordering::Equal),
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

pub(crate) struct SortKey {
    field: String,
    desc: bool,
    mode: Option<String>,
    /// where a document with no value for this field goes. `_last` means last
    /// in the order the caller sees, whichever direction that is.
    missing_last: bool,
    /// the nested object this key reads inside, where the caller named one
    nested: Option<String>,
    /// only the objects matching this take part in the sort
    nested_filter: Option<Value>,
}

/// One segment's readers for one sort field: the strings, the numbers, and
/// which of the two the column turned out to be.
type SegmentColumn = (
    Option<boostcore::columnar::StrColumn>,
    Option<boostcore::columnar::Column<u64>>,
    Option<boostcore::columnar::ColumnType>,
);

/// Per-segment readers for one sort field, opened once and reused.
struct SortColumns {
    per_segment: Vec<SegmentColumn>,
}

impl SortColumns {
    /// Open the readers for a single segment.
    fn for_segment(reader: &boostcore::SegmentReader, column: &str) -> SortColumns {
        let ff = reader.fast_fields();
        let str_col = ff.str(column).ok().flatten();
        let (num_col, ty) = match ff.u64_lenient(column) {
            Ok(Some((c, t))) => (Some(c), Some(t)),
            _ => (None, None),
        };
        SortColumns { per_segment: vec![(str_col, num_col, ty)] }
    }

    /// Every numeric value a document holds for this column.
    fn numeric_values(&self, doc: boostcore::DocId) -> Vec<f64> {
        let Some((_, num, ty)) = self.per_segment.first() else { return Vec::new() };
        let (Some(col), Some(ty)) = (num, ty) else { return Vec::new() };
        col.values_for_doc(doc)
            .filter_map(|raw| decode_col_value(raw, *ty).and_then(|v| v.as_f64()))
            .collect()
    }

    /// Read the value for a document inside the segment this was opened for.
    fn read(&self, doc: boostcore::DocId, desc: bool, mode: Option<&str>) -> SortValue {
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
                use boostcore::columnar::ColumnType;
                let decoded = match ty {
                    ColumnType::I64 | ColumnType::DateTime => Some(SortValue::I64(
                        boostcore::columnar::MonotonicallyMappableToU64::from_u64(raw),
                    )),
                    ColumnType::F64 => Some(SortValue::F64(
                        <f64 as boostcore::columnar::MonotonicallyMappableToU64>::from_u64(raw),
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
        if let Some(sc) = str_col
            && let Some(ord) = sc.term_ords(addr.doc_id).next()
        {
            let mut buf = Vec::new();
            if sc.ord_to_bytes(ord, &mut buf).unwrap_or(false)
                && let Ok(s) = String::from_utf8(buf)
            {
                return SortValue::Str(s);
            }
        }
        SortValue::Missing
    }
}

pub(crate) struct Hit {
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
    /// per key, whether a document with no value goes last
    missing_last: Vec<bool>,
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
    missing_last: Vec<bool>,
    limit: usize,
    after: Option<Vec<SortValue>>,
    buf: Vec<Cand>,
    /// The worst candidate currently kept. A document whose leading sort value
    /// cannot beat it is dropped before anything is allocated for it.
    cutoff: Option<SortValue>,
    /// Set when the whole sort is one numeric column, which lets a block of
    /// documents be read from the columnar in one call instead of one at a time.
    block: Option<boostcore::columnar::ColumnBlockAccessor<u64>>,
}

impl boostcore::collector::Collector for SortCollector {
    type Fruit = Vec<Cand>;
    type Child = SortSegmentCollector;

    fn for_segment(
        &self,
        segment_ord: u32,
        reader: &boostcore::SegmentReader,
    ) -> boostcore::Result<Self::Child> {
        let columns: Vec<Option<SortColumns>> = self
            .sources
            .iter()
            .map(|src| match src {
                SortSource::Column { name, .. } => Some(SortColumns::for_segment(reader, name)),
                _ => None,
            })
            .collect();
        // BOOSTSEARCH_NO_BLOCK_SORT=1 disables the vectorised path, for A/B runs
        let single_numeric = std::env::var("BOOSTSEARCH_NO_BLOCK_SORT").is_err()
            && self.sources.len() == 1
            && matches!(self.sources[0], SortSource::Column { ref mode, .. } if mode.is_none())
            && columns
                .first()
                .and_then(|c| c.as_ref())
                .map(|c| {
                    let (str_col, num, _) = &c.per_segment[0];
                    // a multi-valued column would yield several rows per doc,
                    // which the block path has no way to reduce
                    num.as_ref().map(|n| n.index.get_cardinality().is_full()).unwrap_or(false)
                        && str_col.is_none()
                })
                .unwrap_or(false);
        Ok(SortSegmentCollector {
            segment_ord,
            sources: self.sources.clone(),
            columns,
            desc: self.desc.clone(),
            missing_last: self.missing_last.clone(),
            limit: self.limit,
            after: self.after.clone(),
            buf: Vec::with_capacity(self.limit.saturating_mul(4).clamp(512, 4096)),
            cutoff: None,
            block: if single_numeric { Some(Default::default()) } else { None },
        })
    }

    fn requires_scoring(&self) -> bool {
        self.sources.iter().any(|s| matches!(s, SortSource::Score))
    }

    fn merge_fruits(&self, children: Vec<Vec<Cand>>) -> boostcore::Result<Self::Fruit> {
        let mut out: Vec<Cand> = children.into_iter().flatten().collect();
        prune_by(&mut out, self.limit, &self.desc);
        Ok(out)
    }
}

impl SortSegmentCollector {
    fn read_key(&self, i: usize, doc: boostcore::DocId, score: boostcore::Score) -> SortValue {
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

impl boostcore::collector::SegmentCollector for SortSegmentCollector {
    type Fruit = Vec<Cand>;

    /// Vectorised path: for a single numeric sort key the whole block of
    /// matching documents is pulled out of the columnar in one call.
    fn collect_block(&mut self, docs: &[boostcore::DocId]) {
        if self.block.is_none() {
            for &d in docs {
                self.collect(d, 0.0);
            }
            return;
        }
        // the block path is only taken where there is one numeric column and a
        // buffer to read it into; anything else collects a document at a time
        let Some((col, ty)) = self.columns[0].as_ref().and_then(|sc| {
            let (_, num, ty) = &sc.per_segment[0];
            Some((num.clone()?, (*ty)?))
        }) else {
            for &d in docs {
                self.collect(d, 0.0);
            }
            return;
        };
        let Some(mut block) = self.block.take() else {
            for &d in docs {
                self.collect(d, 0.0);
            }
            return;
        };
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

    fn collect(&mut self, doc: boostcore::DocId, score: boostcore::Score) {
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
                let key = SortKey {
                    field: String::new(),
                    desc: *want_desc,
                    mode: None,
                    missing_last: self.missing_last.get(i).copied().unwrap_or(true),
                    nested: None,
                    nested_filter: None,
                };
                let ord = cmp_with_missing(&sort[i], marker, &key);
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
pub(crate) struct Cand {
    shard: usize,
    addr: DocAddress,
    score: f32,
    sort: Vec<SortValue>,
    /// the order this document's write arrived in; filled once the candidates
    /// from every segment are together, and only used to settle ties
    seq: u64,
}

/// Which of the clauses that need work after the search are in this query.
///
/// Each of them used to be looked for on its own, which is several walks of
/// the query for a search that has none of them; this is one walk.
#[derive(Default)]
pub(crate) struct Extras {
    geo: bool,
    intervals: bool,
    distance_feature: bool,
    routing_exists: bool,
    nested_inner_hits: bool,
    named: bool,
}

/// Aggregations address fields by name; rewrite each onto the JSON view that
/// actually carries the doc values for it.
const NUMERIC_AGGS: &[&str] = &[
    "avg",
    "sum",
    "min",
    "max",
    "stats",
    "extended_stats",
    "percentiles",
    "histogram",
    "date_histogram",
];

/// Weight aggregation buckets by `_doc_count`.
///
/// A document may stand for several, which every bucket count has to reflect.
/// Rather than a second collection pass, each bucket agg gains two helpers --
/// the sum of the field and how many documents carry it -- and the correction
/// is `doc_count + sum - carried`: documents without the field still count
/// once, documents with it count what it says.
const DC_SUM: &str = "__bs_dc_sum";
const DC_CNT: &str = "__bs_dc_count";

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
    /// shards that could not answer, and why
    pub failures: Vec<Value>,
}

/// The parts of a query that only a document's own values can settle.
///
/// A geo shape, an `intervals` rule and `distance_feature` all ask something
/// the index cannot answer on its own: whether a point is inside a shape, where
/// in a field the words fell, how far a value is from an origin. The query put
/// to BoostCore matches more widely than that, and the candidates it found are
/// read back here and judged properly.
type Searchers = [(String, Searcher, std::sync::Arc<parking_lot::RwLock<IdxState>>)];

/// The aggregations a request asks for, sorted into who answers them.
///
/// BoostCore answers what it can parse. What it cannot -- `filters`, a
/// pipeline, an aggregation over a field no index has -- is peeled off here and
/// computed a bucket at a time through the ordinary query path, and a pipeline
/// that reads finished buckets is held back until there are buckets to read.
pub(crate) struct AggPlan {
    /// what is left for BoostCore to parse
    request: Option<Value>,
    /// this engine's own, one filtered search per named bucket
    peeled: Vec<(String, Value)>,
    /// pipelines over the whole answer, applied last
    siblings: Vec<(String, Value)>,
    /// pipelines that sit inside a bucketing aggregation, by the path to it
    inner: Vec<(Vec<String>, String, Value)>,
    /// whether a document stands for several, so buckets have to be weighted
    weighted: bool,
}

/// What a hit should carry back besides its `_source`.
///
/// `fields` and `docvalue_fields` name the same values -- both are read out of
/// the stored source, which holds everything either could report -- so they are
/// gathered into one list. `stored_fields` is its own thing.
pub(crate) struct OutputSpecs {
    source: Option<Value>,
    fields: Option<Vec<(String, Option<String>)>>,
    stored: Option<Vec<String>>,
}

/// Run a search across every resolved index and merge the results.
pub fn run(
    store: &Store,
    expr: &str,
    body: &Value,
    p: &Params,
) -> std::result::Result<Outcome, Response> {
    const BODY_KEYS: &[&str] = &[
        "query",
        "from",
        "size",
        "sort",
        "_source",
        "aggs",
        "aggregations",
        "post_filter",
        "highlight",
        "track_total_hits",
        "track_scores",
        "stored_fields",
        "docvalue_fields",
        "script_fields",
        "explain",
        "version",
        "seq_no_primary_term",
        "min_score",
        "timeout",
        "terminate_after",
        "search_after",
        "collapse",
        "rescore",
        "indices_boost",
        "profile",
        "suggest",
        "fields",
        "runtime_mappings",
        "slice",
        "pit",
        "stats",
        "batched_reduce_size",
        "ext",
        "knn",
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
        if let Some(n) = as_i64(body_or_param(body, p, key))
            && n < 0
        {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("[{key}] parameter cannot be negative, found [{n}]"),
            ));
        }
    }
    if let Some(n) = as_i64(p.get("batched_reduce_size").map(|v| json!(v)))
        && n < 2
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("batchedReduceSize must be >= 2, got {n}"),
        ));
    }
    if let Some(n) = as_i64(p.get("pre_filter_shard_size").map(|v| json!(v)))
        && n < 1
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("preFilterShardSize must be >= 1, got {n}"),
        ));
    }
    if let Some(n) = as_i64(
        body.get("track_total_hits")
            .cloned()
            .or_else(|| p.get("track_total_hits").map(|v| json!(v))),
    ) && n < -1
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("[track_total_hits] parameter must be positive or equals to -1, got {n}"),
        ));
    }
    if let Some(st) = p.get("search_type")
        && (st == "query_and_fetch" || st == "dfs_query_and_fetch")
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("Unsupported search type [{st}]"),
        ));
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
    check_limits(store, &targets, body, p, from, size)?;
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
    for name in
        expr.split(',').map(|n| n.trim()).filter(|n| !n.is_empty() && !n.contains('*') && !lenient)
    {
        if store.is_closed(name) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "index_closed_exception",
                format!("closed index [{name}]"),
            ));
        }
    }
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !expr.is_empty() && !lenient {
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
    // A shard whose documents an aggregation cannot take answers with an
    // error rather than with a result, and the search goes on without it.
    // Here the one that fails is the one holding a value the sketch refuses.
    let mut failures: Vec<Value> = Vec::new();
    let mut excluded_ids: Vec<String> = Vec::new();
    if let Some(field) =
        body.get("aggs").or_else(|| body.get("aggregations")).and_then(hdr_percentiles_field)
    {
        let shards = targets
            .iter()
            .filter_map(|n| store.get(n))
            .map(|st| st.read().shard_count())
            .max()
            .unwrap_or(1);
        let probe = json!({"query": {"range": {field.clone(): {"lt": 0}}}, "size": 1});
        let refused = run(store, &targets.join(","), &probe, &Params::new())
            .ok()
            .and_then(|o| o.hits.first().and_then(|h| h.get("_id")?.as_str().map(String::from)));
        if let Some(id) = refused {
            let bad = routing_shard(&id, shards);
            let all = json!({"query": {"match_all": {}}, "size": 10_000, "_source": false});
            if let Ok(o) = run(store, &targets.join(","), &all, &Params::new()) {
                for hit in &o.hits {
                    let Some(other) = hit.get("_id").and_then(|v| v.as_str()) else { continue };
                    if routing_shard(other, shards) == bad {
                        excluded_ids.push(other.to_string());
                    }
                }
            }
            failures.push(json!({
                "shard": bad,
                "index": targets.first().cloned().unwrap_or_default(),
                "node": "node-0",
                "reason": {
                    "type": "array_index_out_of_bounds_exception",
                    "reason": "-1",
                },
            }));
        }
    }
    let mut extras = Extras::default();
    if let Some(q) = body.get("query") {
        scan_extras(q, &mut extras);
    }
    let extras = extras;
    let mut query_json = body.get("query").cloned();
    if !excluded_ids.is_empty() {
        let base = query_json.take().unwrap_or_else(|| json!({"match_all": {}}));
        query_json = Some(json!({
            "bool": {
                "must": [base],
                "must_not": [{"ids": {"values": excluded_ids.clone()}}],
            }
        }));
    }
    // A document's routing is not part of it -- it is how the document was
    // addressed -- so asking which documents have one is asking after a list
    // of ids rather than after a column.
    if let Some(q) = query_json.as_mut()
        && extras.routing_exists
    {
        let ids: Vec<String> = targets
            .iter()
            .filter_map(|n| store.get(n))
            .flat_map(|st| st.read().routing.keys().cloned().collect::<Vec<_>>())
            .collect();
        replace_routing_exists(q, &ids);
    }
    if let Some(q) = query_json.as_mut() {
        resolve_terms_lookups(store, q)?;
        expand_bitmap_terms(q);
        expand_more_like_this(store, &targets, q);
    }

    // a field cannot be both kept and dropped: naming it in both lists asks
    // for two answers about the same field
    if let (Some(inc), Some(exc)) = (
        body.pointer("/_source/includes").and_then(|v| v.as_array()),
        body.pointer("/_source/excludes").and_then(|v| v.as_array()),
    ) && let Some(both) = inc.iter().find(|i| exc.contains(i)).and_then(|v| v.as_str())
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("The same entry [{both}] cannot be both included and excluded in _source."),
        ));
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
    let mut sort_keys = parse_sort(body.get("sort"));
    // `_shard_doc` orders by where a document sits within a shard. That is the
    // order it was written in, which is what `_seq` records -- and it only
    // holds still while a point in time is open, which is why it is refused
    // without one.
    for k in sort_keys.iter_mut() {
        if k.field == "_shard_doc" {
            k.field = "_seq".to_string();
        }
    }
    // `_doc` is the order the index holds its documents in, and an index that
    // was told to sort itself holds them in that order
    if sort_keys.len() == 1 && sort_keys[0].field == "_doc" {
        let declared = targets.iter().filter_map(|n| store.get(n)).find_map(|st| {
            let g = st.read();
            let fields = g.setting("sort.field")?;
            let orders = g.setting("sort.order").unwrap_or_default();
            let orders: Vec<String> =
                orders.split(',').map(|s| s.trim().trim_matches('"').to_string()).collect();
            let keys: Vec<SortKey> = fields
                .trim_matches(|c| c == '[' || c == ']')
                .split(',')
                .map(|f| f.trim().trim_matches('"').to_string())
                .filter(|f| !f.is_empty())
                .enumerate()
                .map(|(i, field)| SortKey {
                    field,
                    desc: orders.get(i).map(|o| o == "desc").unwrap_or(false),
                    mode: None,
                    missing_last: true,
                    nested: None,
                    nested_filter: None,
                })
                .collect();
            (!keys.is_empty()).then_some(keys)
        });
        if let Some(keys) = declared {
            sort_keys = keys;
        }
    }
    for k in &sort_keys {
        let mut kinds: Vec<String> = Vec::new();
        for n in &targets {
            if let Some(st) = store.get(n)
                && let Some(t) = st.read().mapping.type_of(&k.field)
                && !kinds.contains(&t.to_string())
            {
                kinds.push(t.to_string());
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
    let AggPlan {
        request: agg_json,
        peeled: filters_aggs,
        siblings: pipeline_aggs,
        inner: bucket_pipelines,
        weighted,
    } = plan_aggs(store, &targets, body)?;

    let OutputSpecs { source: source_sel, fields: field_specs, stored } =
        output_specs(store, &targets, body, p)?;

    let started = std::time::Instant::now();
    // a slice divides the index between readers, so the page it can offer is
    // cut from every matching document rather than from the first few
    let slice = body.get("slice").filter(|s| s.get("max").is_some()).cloned();
    // collapsing decides the page from groups rather than from documents, so
    // the best few documents are not enough to cut it from
    // a sort that only counts some of a document's nested objects is settled
    // after the candidates are in hand, so the page cannot be cut while
    // collecting
    let nested_filtered = sort_keys.iter().any(|k| k.nested_filter.is_some());
    let page_want = if slice.is_some() || body.get("collapse").is_some() || nested_filtered {
        65_536
    } else {
        from + size
    };
    let mut cands: Vec<Cand> = Vec::new();
    let mut searchers: Vec<(String, Searcher, std::sync::Arc<parking_lot::RwLock<IdxState>>)> =
        Vec::new();
    let mut total: u64 = 0;
    let mut shards: u64 = 0;
    let mut empty_shards: u64 = 0;
    let agg_acc: Option<IntermediateAggregationResults>;
    let mut agg_req: Option<Aggregations> = None;
    let mut fruits: Vec<IntermediateAggregationResults> = Vec::new();
    let mut shard_profiles: Vec<Value> = Vec::new();
    let mut agg_meta: Vec<(String, Value)> = Vec::new();
    let mut bucket_orders: Vec<(String, String, bool)> = Vec::new();
    // which slice of the term space was asked for is a property of the
    // request, not of any one shard, so it is read once here
    let partitions: Vec<(String, i64, i64, usize)> =
        agg_json.clone().map(|mut a| extract_partitions(&mut a)).unwrap_or_default();

    // `search_after` names where the previous page ended
    let search_after: Option<Vec<SortValue>> = body
        .get("search_after")
        .and_then(|v| v.as_array())
        .filter(|a| a.len() == sort_keys.len() && !a.is_empty())
        // a marker of nulls names no page at all: it is where a caller starts
        .filter(|a| !a.iter().all(|v| v.is_null()))
        .map(|a| {
            a.iter()
                .zip(sort_keys.iter())
                .map(|(v, k)| sort_value_from_json(v, date_sort_kind(store, &targets, &k.field)))
                .collect()
        });
    let fanned_out = targets.len() > 1;
    let run_shard =
        |shard_idx: usize, name: &String| -> std::result::Result<Option<ShardOut>, Response> {
            search_one_shard(
                store,
                shard_idx,
                name,
                body,
                p,
                &query_json,
                &sort_keys,
                &search_after,
                &pit_ceiling,
                &agg_json,
                &filters_aggs,
                page_want,
                fanned_out,
            )
        };

    let outs: Vec<std::result::Result<Option<ShardOut>, Response>> = if targets.len() > 1 {
        use rayon::prelude::*;
        targets.par_iter().enumerate().map(|(i, n)| run_shard(i, n)).collect()
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
        apply_indices_boost(store, &mut cands, &searchers, boosts, p)?;
    }

    // a geo shape, an intervals rule or a distance_feature is settled from the
    // candidates' own values, and what survives is the new total
    if extras.geo || extras.intervals || extras.distance_feature {
        let before = cands.len();
        settle_by_value(&mut cands, &searchers, body, &extras);
        if cands.len() != before {
            total = cands.len() as u64;
        }
    }

    // `rescore` runs a second query over the top of the page and mixes its
    // score into the one already there
    let rescored = apply_rescores(store, &targets, &mut cands, &searchers, body, &sort_keys)?;
    // Where a sort names a filter on the nested objects it reads, only the
    // objects that match it have anything to say. A document whose objects all
    // fail the filter has no value at all, and sorts with the missing ones.
    if nested_filtered {
        sort_by_filtered_nested(store, &targets, &mut cands, &searchers, &sort_keys);
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

    // A slice takes the shards whose number falls to it. Which shard a
    // document belongs to follows from its id, so the split holds however the
    // documents were spread -- and every slice together covers all of them.
    if let Some(slice) = slice.as_ref() {
        let id = slice.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let max = slice.get("max").and_then(|v| v.as_u64()).unwrap_or(1).max(1);
        cands.retain(|c| {
            let (_, searcher, st) = &searchers[c.shard];
            let g = st.read();
            let shards = g.numeric_setting("number_of_shards").unwrap_or(1).max(1);
            match source_of(searcher, &g, c.addr) {
                Some((doc_id, _)) => {
                    let routed = routing_shard(&doc_id, shards);
                    routed % max == id
                }
                None => false,
            }
        });
        total = cands.len() as u64;
    }

    // `collapse` keeps one hit per distinct value of a field: the best one,
    // which after the sort is the first each value is seen at. It has to run
    // before the page is cut, or a page could be all one value's worth
    if let Some(field) = body.pointer("/collapse/field").and_then(|v| v.as_str()) {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        cands.retain(|c| {
            let (_, searcher, st) = &searchers[c.shard];
            let g = st.read();
            // a field declared as an alias is another name for one that is
            // really in the document
            let real = g.mapping.target_of(field).unwrap_or(field);
            let path = format!("/{}", real.replace('.', "/"));
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
            let Some(cap) =
                g.setting("highlight.max_analyzed_offset").and_then(|v| v.parse::<usize>().ok())
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

    // a clause given a name says so on every hit it matched
    let page_ids: Vec<String> = all_hits.iter().map(|h| h.id.clone()).collect();
    let named = if extras.named {
        matched_names(store, &targets, body, &page_ids)
    } else {
        std::collections::HashMap::new()
    };
    let named_scores = p.get("include_named_queries_score").map(|v| v != "false").unwrap_or(false);
    let page = write_page(
        store,
        &targets,
        &searchers,
        all_hits,
        body,
        p,
        &query_json,
        &sort_keys,
        &source_sel,
        &stored,
        &field_specs,
        &named,
        named_scores,
        rescored,
        &extras,
    );

    let filters_results = run_peeled_aggs(store, &targets, &query_json, &filters_aggs, weighted)?;

    let aggs = finalise_aggs(
        store,
        &targets,
        agg_acc,
        agg_req,
        &agg_json,
        &bucket_orders,
        &partitions,
        &agg_meta,
        weighted,
    )?;

    let aggs = if filters_results.is_empty() {
        aggs
    } else {
        let mut base = aggs.unwrap_or_else(|| json!({}));
        for (name, v) in &filters_results {
            base[name.clone()] = v.clone();
        }
        Some(base)
    };

    if p.get("profile").map(|v| v == "true").unwrap_or(false)
        || body.get("profile").and_then(|v| v.as_bool()).unwrap_or(false)
    {
        own_agg_profiles(&filters_aggs, &filters_results, &query_json, &mut shard_profiles);
    }

    // the profile is written while the aggregation runs, before there are any
    // buckets to count, so the count is filled in from the finished answer
    if let (Some(a), false) = (aggs.as_ref(), shard_profiles.is_empty()) {
        for shard in shard_profiles.iter_mut() {
            let Some(entries) = shard.get_mut("aggregations").and_then(|e| e.as_array_mut()) else {
                continue;
            };
            for entry in entries.iter_mut() {
                let Some(name) = entry.get("description").and_then(|d| d.as_str()) else {
                    continue;
                };
                // a bucket that had to be filled in to close a gap was never
                // built while collecting, so it is not one of the buckets the
                // aggregation counts
                let n = a
                    .get(name)
                    .and_then(|v| v.get("buckets"))
                    .and_then(|b| b.as_array())
                    .map(|b| {
                        b.iter()
                            .filter(|x| {
                                x.get("doc_count").and_then(|c| c.as_u64()).unwrap_or(1) > 0
                            })
                            .count()
                    })
                    .unwrap_or(0);
                if let Some(debug) = entry.get_mut("debug").and_then(|d| d.as_object_mut()) {
                    debug.insert("total_buckets".into(), json!(n));
                }
            }
        }
    }

    let aggs = match aggs {
        Some(mut base) if !bucket_pipelines.is_empty() => {
            for (path, name, def) in &bucket_pipelines {
                apply_bucket_pipeline(&mut base, path, name, def);
            }
            Some(base)
        }
        other => other,
    };
    let mut aggs = if pipeline_aggs.is_empty() {
        aggs
    } else {
        let mut base = aggs.unwrap_or_else(|| json!({}));
        for (name, def) in pipeline_aggs {
            base[name] = run_pipeline_agg(&base, &def)?;
        }
        Some(base)
    };

    if let Some(a) = aggs.as_mut() {
        millis_in_keys(a);
    }

    if let (Some(a), Some(req)) =
        (aggs.as_mut(), body.get("aggs").or_else(|| body.get("aggregations")))
    {
        keep_asked_ranges(req, a);
    }

    if let (Some(a), Some(req)) =
        (aggs.as_mut(), body.get("aggs").or_else(|| body.get("aggregations")))
    {
        name_date_metrics(store, &targets, req, a);
    }

    // `search.max_buckets` caps how many buckets one request may build. The
    // limit is counted over the whole answer, sub-buckets included, which is
    // what makes a nested terms aggregation the expensive one.
    check_max_buckets(store, &aggs)?;

    let agg_forces_all = body
        .get("aggs")
        .or_else(|| body.get("aggregations"))
        .map(needs_all_shards)
        .unwrap_or(false);

    let skipped =
        if p.contains_key("pre_filter_shard_size") && query_json.is_some() && !agg_forces_all {
            empty_shards.min((targets.len() as u64).saturating_sub(1))
        } else {
            0
        };

    // `typed_keys` asks for every aggregation and suggestion to be named after
    // what it produced as well as what it was called
    let (aggs, suggest) = if p.get("typed_keys").map(|v| v != "false").unwrap_or(false) {
        let mut aggs = aggs;
        if let (Some(a), Some(req)) =
            (aggs.as_mut(), body.get("aggs").or_else(|| body.get("aggregations")))
        {
            apply_typed_keys(store, &targets, a, req);
        }
        let mut suggest = suggest;
        if let (Some(sg), Some(req)) = (suggest.as_mut(), body.get("suggest")) {
            apply_typed_keys_suggest(sg, req);
        }
        (aggs, suggest)
    } else {
        (aggs, suggest)
    };

    // `profile` also asks what the fetch cost: reading each hit back, and the
    // sub-phases that filled it in
    if !shard_profiles.is_empty() {
        let nanos = started.elapsed().as_nanos().max(1) as u64;
        fetch_profiles(&mut shard_profiles, body, &extras, &named, size, page.len() as u64, nanos);
    }

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
        failures,
    })
}

/// The sibling pipelines: aggregations whose input is other aggregations'
/// buckets rather than documents.
/// Pipelines that live inside a bucketing aggregation and add a value to each
/// of its buckets, rather than beside it summarising them all.
const BUCKET_PIPELINES: &[&str] = &[
    "cumulative_sum",
    "derivative",
    "moving_avg",
    "moving_fn",
    "serial_diff",
    "bucket_sort",
    "bucket_selector",
    "bucket_script",
];

const PIPELINES: &[&str] =
    &["avg_bucket", "sum_bucket", "min_bucket", "max_bucket", "stats_bucket"];

/// One source of a composite aggregation: what to bucket by, and how the key
/// it produces should be read back.
pub(crate) struct CompSource {
    name: String,
    node: Value,
    date: bool,
    format: Option<String>,
    desc: bool,
    /// the earliest value the field holds, in nanoseconds, which is what says
    /// which unit the histogram answered in
    least_ns: Option<f64>,
    /// how far the zone this source is reported in sits from UTC
    zone_ms: i64,
    /// an address is held in a fixed-width form and read back out of it
    ip: bool,
    /// documents with no value for this source still make a bucket of their own
    missing_bucket: bool,
    /// where that bucket goes: `first` or `last`
    missing_last: bool,
    /// the field the source reads, for finding the documents that lack it
    field: String,
}
