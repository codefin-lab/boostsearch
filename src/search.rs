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
}

struct SortSegmentCollector {
    segment_ord: u32,
    sources: Vec<SortSource>,
    columns: Vec<Option<SortColumns>>,
    desc: Vec<bool>,
    limit: usize,
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
        for (doc, raw) in block.iter_docid_vals(docs, &col) {
            let Some(v) = decode_col_value(raw, ty) else { continue };
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
        self.buf.push(Cand { shard: 0, addr: DocAddress::new(self.segment_ord, doc), score, sort });
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
}

fn cmp_cands(a: &Cand, b: &Cand, sort_keys: &[SortKey]) -> Ordering {
    // ties fall back to document order, which is insertion order within a
    // shard -- otherwise equally-scored hits come back in a different order
    // from one run to the next
    let by_doc = || {
        a.shard
            .cmp(&b.shard)
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
        }
        _ => {}
    }
    Ok(())
}

fn check_agg_node(node: &Value, ctx: &Ctx, owner: &str) -> std::result::Result<(), Response> {
    let Some(o) = node.as_object() else { return Ok(()) };
    for (name, def) in o {
        check_agg_params(name, def, owner)?;
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
fn resolve_terms_lookups(store: &Store, node: &mut Value) -> std::result::Result<(), Response> {
    match node {
        Value::Object(o) => {
            if let Some(Value::Object(spec)) = o.get("terms").cloned().map(|v| v) {
                for (field, def) in spec {
                    let Some(d) = def.as_object() else { continue };
                    let (Some(index), Some(id), Some(path)) = (
                        d.get("index").and_then(|v| v.as_str()),
                        d.get("id").and_then(|v| v.as_str()),
                        d.get("path").and_then(|v| v.as_str()),
                    ) else {
                        continue;
                    };
                    let Some(st) = store.get(index) else {
                        return Err(no_such_index(index));
                    };
                    let g = st.read();
                    let values = crate::api::read_source(&g, id)
                        .and_then(|src| {
                            src.pointer(&format!("/{}", path.replace('.', "/"))).cloned()
                        })
                        .unwrap_or(Value::Array(vec![]));
                    let list = match values {
                        Value::Array(a) => a,
                        other => vec![other],
                    };
                    o.insert("terms".into(), json!({ field: list }));
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
) -> (tantivy::Result<IntermediateAggregationResults>, Value) {
    use std::time::Instant;
    use tantivy::collector::{Collector, SegmentCollector};

    let mut ns = std::collections::BTreeMap::new();
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

    let entries: Vec<Value> = aggs
        .iter()
        .map(|(name, agg)| {
            let def = serde_json::to_value(agg).unwrap_or(Value::Null);
            json!({
                "type": agg_profile_type(&def),
                "description": name,
                "time_in_nanos": total,
                "breakdown": ns.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<serde_json::Map<_, _>>(),
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
        "terms" => "GlobalOrdinalsStringTermsAggregator".into(),
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

pub struct Outcome {
    pub took_ms: u64,
    pub skipped: u64,
    pub shards: u64,
    pub total: u64,
    pub hits: Vec<Value>,
    pub max_score: Option<f32>,
    pub aggs: Option<Value>,
    pub profile: Option<Value>,
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
    let targets = store.resolve(expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !expr.is_empty() {
        return Err(no_such_index(expr));
    }
    // a `terms` lookup names a document to read the term list from
    let mut query_json = body.get("query").cloned();
    if let Some(q) = query_json.as_mut() {
        if let Err(r) = resolve_terms_lookups(store, q) {
            return Err(r);
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
                    || def.get("weighted_avg").is_some()
                    || def.get("auto_date_histogram").is_some()
                    || def.get("variable_width_histogram").is_some()
                    // calendar units are not fixed lengths, which is all
                    // tantivy's date histogram knows how to step by
                    || def
                        .get("date_histogram")
                        .map(|d| d.get("calendar_interval").is_some())
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
    let field_specs: Option<Vec<(String, Option<String>)>> =
        match (spec_list(body.get("fields")), spec_list(body.get("docvalue_fields"))) {
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
        if let Some(aj) = &agg_json {
            let mut rewritten = aj.clone();
            normalize_aggs(&mut rewritten, &mut agg_meta, true);
            check_agg_types(&rewritten, &ctx)?;
            normalize_agg_dates(&mut rewritten);
            bucket_orders = extract_bucket_orders(&mut rewritten);
            lower_nested_filters(&mut rewritten, &ctx);
            rewrite_agg_fields(&mut rewritten, &ctx);
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
                SortCollector { sources, desc, limit: want.max(1) },
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
            let (res, prof) = profiled_agg_search(&searcher, &q, a.clone(), ctxp, &ctx);
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
    cands.sort_by(|a, b| cmp_cands(a, b, &sort_keys));

    let max_score = if sort_keys.is_empty() {
        cands.iter().map(|c| c.score).fold(None::<f32>, |acc, s| Some(acc.map_or(s, |a| a.max(s))))
    } else {
        None
    };

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
            let sel = source_sel.clone().or_else(|| crate::api::source_selector_from_params_pub(p));
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
                hit["sort"] = Value::Array(h.sort.iter().map(|s| s.to_json()).collect());
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
            if body.get("version").and_then(|v| v.as_bool()).unwrap_or(false) {
                hit["_version"] = json!(h.version);
            }
            if body.get("seq_no_primary_term").and_then(|v| v.as_bool()).unwrap_or(false) {
                hit["_seq_no"] = json!(0);
                hit["_primary_term"] = json!(1);
            }
            hit
        })
        .collect();

    let mut filters_results: Vec<(String, Value)> = Vec::new();
    for (name, def) in &filters_aggs {
        let own_meta = def.get("meta").cloned();
        let outcome = if def.get("missing").is_some() {
            run_missing_agg(store, &targets, &query_json, def)
        } else if def.get("median_absolute_deviation").is_some() {
            run_mad_agg(store, &targets, &query_json, def)
        } else if def.get("percentiles").is_some() {
            run_hdr_percentiles(store, &targets, &query_json, def)
        } else if def.get("filter").is_some() {
            run_filter_agg(store, &targets, &query_json, def)
        } else if def.get("global").is_some() {
            // `global` ignores the query and aggregates over every document
            run_filter_agg(store, &targets, &None, &json!({
                "filter": {"match_all": {}},
                "aggs": def.get("aggs").or_else(|| def.get("aggregations")).cloned()
                    .unwrap_or_else(|| json!({}))
            }))
        } else if def.get("weighted_avg").is_some() {
            run_weighted_avg(store, &targets, &query_json, def)
        } else if def.get("variable_width_histogram").is_some() {
            run_variable_width_histogram(store, &targets, &query_json, def)
        } else if def.get("auto_date_histogram").is_some() {
            run_auto_date_histogram(store, &targets, &query_json, def)
        } else if def.get("composite").is_some() {
            run_composite_agg(store, &targets, &query_json, def, weighted)
        } else if def.get("date_histogram").is_some() {
            run_calendar_histogram(store, &targets, &query_json, def)
        } else if def.get("terms").is_some() {
            run_index_terms_agg(store, &targets, &query_json, def)
        } else {
            run_filters_agg(store, &targets, &query_json, def)
        };
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
                if weighted {
                    apply_doc_counts(&mut v);
                }
                apply_bucket_orders(&mut v, &bucket_orders);
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

    let aggs = if pipeline_aggs.is_empty() {
        aggs
    } else {
        let mut base = aggs.unwrap_or_else(|| json!({}));
        for (name, def) in pipeline_aggs {
            base[name] = run_pipeline_agg(&base, &def)?;
        }
        Some(base)
    };

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
        let (count, sub) = filtered_count(store, targets, &combined, &sub_aggs)?;
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
        let (count, sub) = filtered_count(store, targets, &combined, &sub_aggs)?;
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
    let (count, sub) = filtered_count(store, targets, &combined, &sub_aggs)?;
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
    let (count, sub) = filtered_count(store, targets, &combined, &sub_aggs)?;
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
        return Ok(json!({"buckets": []}));
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

    let mut buckets = Vec::new();
    let mut cursor = unit.floor(lo);
    let last = unit.floor(hi);
    // a runaway interval would otherwise spin: no calendar histogram the suite
    // or a sane request produces comes near this
    let mut guard = 0;
    while cursor <= last && guard < 100_000 {
        guard += 1;
        let next = unit.advance(cursor);
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
        let (count, sub) = filtered_count(store, targets, &combined, &sub_aggs)?;
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
    let _ = Duration::seconds(0);
    Ok(json!({"buckets": buckets}))
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
