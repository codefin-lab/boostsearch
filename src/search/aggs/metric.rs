//! The aggregations that answer with a number rather than with buckets.

use super::*;
use crate::search::*;

/// The field an HDR percentiles aggregation reads, if the request has one.
pub(crate) fn hdr_percentiles_field(node: &Value) -> Option<String> {
    let o = node.as_object()?;
    for (_, def) in o {
        if let Some(spec) = def.get("percentiles")
            && spec.get("hdr").is_some()
            && let Some(f) = spec.get("field").and_then(|f| f.as_str())
        {
            return Some(f.to_string());
        }
        if let Some(subs) = def.get("aggs").or_else(|| def.get("aggregations"))
            && let Some(f) = hdr_percentiles_field(subs)
        {
            return Some(f);
        }
    }
    None
}

/// Every value of one numeric field across the documents a query matches.
///
/// Aggregations that BoostCore does not provide are computed from these directly;
/// the field is read from the columnar, so nothing is materialised per document
/// beyond the value itself.
pub(crate) fn collect_field_values(
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
            analysis: &g.analysis,
            index: &g.index,
            max_terms_count: g.max_terms_count(),
            max_regex_length: g.max_regex_length(),
            allow_expensive: crate::search::expensive_allowed(store),
            observed_kinds: &g.observed_kinds,
            kinds_complete: g.kinds_complete,
            stats: &g.stats,
        };
        let q = crate::query::build(&ctx, query_json)
            .map_err(|e| err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string()))?;
        let column = ctx.column_name(field, false);
        let searcher = g.reader.searcher();
        let addrs = searcher.search(&q, &boostcore::collector::DocSetCollector).map_err(|e| {
            err(StatusCode::BAD_REQUEST, "search_phase_execution_exception", e.to_string())
        })?;
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
            if !any && let Some(m) = missing {
                out.push(m);
            }
        }
    }
    Ok(out)
}

pub(crate) fn agg_field_and_missing(spec: &Value) -> (String, Option<f64>) {
    let field = spec.get("field").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let missing = spec.get("missing").and_then(|v| v.as_f64());
    (field, missing)
}

/// `percentiles` with an `hdr` option, reported the way HdrHistogram does.
pub(crate) fn run_hdr_percentiles(
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
    // without `hdr`, OpenSearch keeps a t-digest, which for the counts these
    // aggregations see holds every value and reports the one at the rank the
    // percentile names
    let hdr = spec.get("hdr").is_some();
    let mut sorted = values.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let at = |p: f64| -> Option<f64> {
        if sorted.is_empty() {
            return None;
        }
        let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
        sorted.get(rank.min(sorted.len()) - 1).copied()
    };
    let mut hist = crate::hdr::HdrHistogram::default();
    for v in &values {
        hist.record(*v);
    }
    let value_at = |p: f64| if hdr { hist.value_at(p) } else { at(p) };

    if keyed {
        let mut map = serde_json::Map::new();
        for p in &percents {
            let key = format!("{p:.1}");
            map.insert(key, value_at(*p).map(|v| json!(v)).unwrap_or(Value::Null));
        }
        Ok(json!({ "values": Value::Object(map) }))
    } else {
        let arr: Vec<Value> =
            percents.iter().map(|p| json!({"key": p, "value": value_at(*p)})).collect();
        Ok(json!({ "values": arr }))
    }
}

/// `weighted_avg`: sum(value * weight) / sum(weight), paired per document.
pub(crate) fn run_weighted_avg(
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
pub(crate) fn collect_field_pairs(
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
            analysis: &g.analysis,
            index: &g.index,
            max_terms_count: g.max_terms_count(),
            max_regex_length: g.max_regex_length(),
            allow_expensive: crate::search::expensive_allowed(store),
            observed_kinds: &g.observed_kinds,
            kinds_complete: g.kinds_complete,
            stats: &g.stats,
        };
        let q = crate::query::build(&ctx, query_json)
            .map_err(|e| err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string()))?;
        let (a_col, b_col) = (ctx.column_name(a_field, false), ctx.column_name(b_field, false));
        let searcher = g.reader.searcher();
        let addrs = searcher.search(&q, &boostcore::collector::DocSetCollector).map_err(|e| {
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

/// `percentile_ranks`: for each value given, how much of the data falls at or
/// below it.
pub(crate) fn run_percentile_ranks(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("percentile_ranks").cloned().unwrap_or(json!({}));
    let (field, missing) = agg_field_and_missing(&spec);
    let wanted: Vec<f64> = spec
        .get("values")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_f64()).collect())
        .unwrap_or_default();
    let keyed = spec.get("keyed").and_then(|v| v.as_bool()).unwrap_or(true);
    let query = combine(main_query, None);
    let mut values = collect_field_values(store, targets, &query, &field, missing)?;
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let rank = |v: f64| -> Option<f64> {
        if values.is_empty() {
            return None;
        }
        let below = values.iter().filter(|x| **x <= v).count();
        Some(below as f64 * 100.0 / values.len() as f64)
    };
    if keyed {
        let mut map = serde_json::Map::new();
        for v in &wanted {
            map.insert(format!("{v:.1}"), rank(*v).map(|r| json!(r)).unwrap_or(Value::Null));
        }
        Ok(json!({"values": Value::Object(map)}))
    } else {
        let arr: Vec<Value> = wanted.iter().map(|v| json!({"key": v, "value": rank(*v)})).collect();
        Ok(json!({"values": arr}))
    }
}

/// `top_hits`: the documents themselves, from inside whatever bucket the
/// aggregation sits in. It is the ordinary search, narrowed and asked again.
pub(crate) fn run_top_hits(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("top_hits").cloned().unwrap_or(json!({}));
    let mut body = json!({"query": main_query.clone().unwrap_or_else(|| json!({"match_all": {}}))});
    for key in [
        "size",
        "from",
        "sort",
        "_source",
        "version",
        "seq_no_primary_term",
        "docvalue_fields",
        "stored_fields",
        "highlight",
        "explain",
        "fields",
        "script_fields",
        "track_scores",
    ] {
        if let Some(v) = spec.get(key) {
            body[key] = v.clone();
        }
    }
    if body.get("size").is_none() {
        body["size"] = json!(3);
    }
    let out = run(store, &targets.join(","), &body, &Params::new())?;
    Ok(json!({"hits": {
        "total": {"value": out.total, "relation": "eq"},
        "max_score": out.max_score.map(|s| json!(s)).unwrap_or(Value::Null),
        "hits": out.hits,
    }}))
}

pub(crate) fn run_mad_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("median_absolute_deviation").cloned().unwrap_or(json!({}));
    if let Some(c) = spec.get("compression").and_then(|v| v.as_f64())
        && c <= 0.0
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("[compression] must be greater than 0. Found [{c:?}] in [mad]"),
        ));
    }
    let (field, missing) = agg_field_and_missing(&spec);
    let query = combine(main_query, None);
    let mut values = collect_field_values(store, targets, &query, &field, missing)?;
    Ok(json!({ "value": crate::hdr::median_absolute_deviation(&mut values) }))
}
