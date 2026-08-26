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
    index: String,
    id: String,
    score: f32,
    source: Value,
    sort: Vec<SortValue>,
    version: u64,
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
        let single_numeric = self.sources.len() == 1
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
    if sort_keys.is_empty() {
        return b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal);
    }
    for (i, k) in sort_keys.iter().enumerate() {
        let ord = a.sort[i].cmp_asc(&b.sort[i]);
        let ord = if k.desc && ord != Ordering::Equal { ord.reverse() } else { ord };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
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
                if matches!(ctx.mapping.type_of(f), Some("text") | Some("keyword")) {
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

fn rewrite_agg_fields(node: &mut Value, ctx: &Ctx) {
    match node {
        Value::Object(o) => {
            if let Some(Value::String(f)) = o.get("field") {
                // `_raw` carries both the untokenised strings and the numerics,
                // so it is the right column for every agg except one over an
                // explicitly analysed text field.
                // `_raw` carries both the untokenised strings and the numerics,
                // so it is the right column for every agg except one over an
                // explicitly analysed text field.
                let analyzed = matches!(ctx.view(f, false), View::Dyn)
                    && ctx.mapping.type_of(f).is_some();
                let prefix = if analyzed { crate::store::DYN } else { crate::store::RAW };
                let base = f.strip_suffix(".keyword").unwrap_or(f);
                let rewritten = format!("{prefix}.{base}");
                o.insert("field".into(), json!(rewritten));
            }
            for (_, v) in o.iter_mut() {
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

pub struct Outcome {
    pub took_ms: u64,
    pub skipped: u64,
    pub shards: u64,
    pub total: u64,
    pub hits: Vec<Value>,
    pub max_score: Option<f32>,
    pub aggs: Option<Value>,
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
    if let Some(a) = agg_json.as_mut() {
        // a filter aggregation can carry a terms lookup too
        resolve_terms_lookups(store, a)?;
    }
    // tantivy has `filter` but not `filters`; peel those out and run them
    // ourselves as one filtered search per named bucket
    let mut filters_aggs: Vec<(String, Value)> = Vec::new();
    if let Some(Value::Object(o)) = agg_json.as_mut() {
        let names: Vec<String> = o
            .iter()
            .filter(|(_, def)| {
                def.get("filters").is_some()
                    || def.get("missing").is_some()
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
    let source_sel = body.get("_source").cloned();
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
    let mut agg_meta: Vec<(String, Value)> = Vec::new();
    let mut bucket_orders: Vec<(String, String, bool)> = Vec::new();

    for (shard_idx, name) in targets.iter().enumerate() {
        let Some(st) = store.get(name) else { continue };
        let g = st.read();
        g.search_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if p.get("request_cache").map(|v| v == "true").unwrap_or(false) {
            g.request_cache_miss.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        shards += g
            .effective_settings()
            .pointer("/index/number_of_shards")
            .and_then(|v| v.as_str())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(1);
        let ctx = Ctx {
            fields: &g.fields,
            mapping: &g.mapping,
            index: &g.index,
            max_terms_count: g.max_terms_count(),
            observed_kinds: &g.observed_kinds,
            kinds_complete: g.kinds_complete,
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
        let (count, shard_cands): (usize, Vec<Cand>) = if sort_keys.is_empty() {
            let collector = (Count, TopDocs::with_limit(want.max(1)).order_by_score());
            match searcher.search(&q, &collector) {
                Ok((c, docs)) => (
                    c,
                    docs.into_iter()
                        .map(|(score, addr)| Cand {
                            shard: shard_idx,
                            addr,
                            score,
                            sort: Vec::new(),
                        })
                        .collect(),
                ),
                Err(e) => {
                    return Err(err(StatusCode::BAD_REQUEST, "search_phase_execution_exception", e.to_string()));
                }
            }
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
            );
            match searcher.search(&q, &collector) {
                Ok((c, mut cands)) => {
                    for cand in cands.iter_mut() {
                        cand.shard = shard_idx;
                    }
                    (c, cands)
                }
                Err(e) => {
                    return Err(err(StatusCode::BAD_REQUEST, "search_phase_execution_exception", e.to_string()));
                }
            }
        };
        total += count as u64;
        if count == 0 {
            empty_shards += 1;
        }
        cands.extend(shard_cands);

        if let Some(a) = this_agg {
            let ctxp = AggContextParams::new(Default::default(), g.index.tokenizers().clone());
            let collector = DistributedAggregationCollector::from_aggs(a.clone(), ctxp);
            match searcher.search(&q, &collector) {
                Ok(res) => {
                    match agg_acc.as_mut() {
                        Some(acc) => {
                            let _ = acc.merge_fruits(res);
                        }
                        None => agg_acc = Some(res),
                    }
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

        searchers.push((g.name.clone(), searcher, st.clone()));
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
        let Some((id, src)) = source_of(searcher, &g, c.addr) else { continue };
        let version = g.version_of(&id);
        all_hits.push(Hit {
            index: name.clone(),
            id,
            score: c.score,
            source: src,
            sort: c.sort,
            version,
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
            if !h.sort.is_empty() {
                hit["sort"] = Value::Array(h.sort.iter().map(|s| s.to_json()).collect());
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
        } else if def.get("filter").is_some() {
            run_filter_agg(store, &targets, &query_json, def)
        } else if def.get("global").is_some() {
            // `global` ignores the query and aggregates over every document
            run_filter_agg(store, &targets, &None, &json!({
                "filter": {"match_all": {}},
                "aggs": def.get("aggs").or_else(|| def.get("aggregations")).cloned()
                    .unwrap_or_else(|| json!({}))
            }))
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
