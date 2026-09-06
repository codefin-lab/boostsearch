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
            vectors: &g.vectors,
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
        // the value whose place that share of the way through is reached,
        // counted the way OpenSearch counts it
        let at = ((p / 100.0) * sorted.len() as f64).floor() as usize;
        sorted.get(at.min(sorted.len() - 1)).copied()
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
            vectors: &g.vectors,
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
        // a value that is there counts for half: the share of the documents
        // below it, plus half of those standing at it
        let under = values.iter().filter(|x| **x < v).count();
        let at = values.iter().filter(|x| **x == v).count();
        let below = under as f64 + at as f64 / 2.0;
        Some(below * 100.0 / values.len() as f64)
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
    // the documents are scored the way the search scores them: a bucket that
    // narrows by a filter alone would otherwise score everything at nought
    let narrowed = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
    let scored = match narrowed.get("bool").and_then(|b| b.as_object()) {
        Some(clauses) if !clauses.contains_key("must") && !clauses.contains_key("should") => {
            let mut with = narrowed.clone();
            with["bool"]["must"] = json!([{"match_all": {}}]);
            with
        }
        _ => narrowed,
    };
    let mut body = json!({ "query": scored });
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

/// The metric aggregations that take a value per document, where the value is
/// worked out by a script rather than read from a field.
pub(crate) const SCRIPTED_METRICS: &[&str] =
    &["min", "max", "sum", "avg", "value_count", "stats", "extended_stats", "cardinality"];

/// Which of them this definition is, where it names a script and no field.
pub(crate) fn scripted_metric_kind(def: &Value) -> Option<&'static str> {
    SCRIPTED_METRICS.iter().copied().find(|k| {
        def.pointer(&format!("/{k}/script")).is_some()
            && def.pointer(&format!("/{k}/field")).is_none()
    })
}

/// A metric over what a script says each document is worth.
///
/// The engine reads a metric out of a column, and a script has no column to
/// read: the documents are walked here instead, the script run over each of
/// them, and the numbers folded the way the named metric folds them.
pub(crate) fn run_scripted_value_metric(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
    kind: &str,
) -> std::result::Result<Value, Response> {
    use crate::painless::contexts::Compiled;
    let spec = def.get(kind).cloned().unwrap_or(json!({}));
    let index = targets.first().cloned().unwrap_or_default();
    let script = spec.get("script").cloned().unwrap_or(json!({}));
    let compiled = Compiled::of(&script, &|id| store.stored_script(id))
        .map_err(|e| crate::search::search_script_failure(e, &index))?;
    let missing = spec.get("missing").and_then(|v| v.as_f64());
    let query = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
    // `track_scores` because `_score` is one of the things such a script is
    // most often written over
    let probe = json!({"query": query, "size": 10_000, "track_scores": true});
    let found = crate::search::run(store, &targets.join(","), &probe, &Params::new())?;
    let mut values: Vec<f64> = Vec::new();
    for hit in &found.hits {
        let name = hit.get("_index").and_then(|v| v.as_str()).unwrap_or("");
        let Some(st) = store.get(name) else { continue };
        let mapping = st.read().mapping.clone();
        let source = hit.get("_source").cloned().unwrap_or(json!({}));
        let expanded = crate::store::expand_for_indexing(source, &mapping);
        let score = hit.get("_score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let out = crate::painless::contexts::run_on_doc(
            &compiled_spec(&script),
            &expanded,
            &mapping,
            score,
        )
        .map_err(|e| crate::search::search_script_failure(e, &index))?;
        match out.to_json() {
            Value::Number(n) => values.extend(n.as_f64()),
            // a script may hand back the several values of a field at once
            Value::Array(a) => values.extend(a.iter().filter_map(|v| v.as_f64())),
            Value::Bool(b) => values.push(f64::from(b)),
            Value::Null => values.extend(missing),
            _ => {}
        }
    }
    Ok(fold_metric(kind, &values))
}

/// The script as `run_on_doc` wants it, with its params carried along.
fn compiled_spec(script: &Value) -> Value {
    match script {
        Value::String(s) => json!({"source": s}),
        other => other.clone(),
    }
}

/// The numbers a metric leaves behind, folded the way that metric folds them.
fn fold_metric(kind: &str, values: &[f64]) -> Value {
    let count = values.len() as u64;
    let sum: f64 = values.iter().sum();
    let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let avg = (count > 0).then(|| sum / count as f64);
    let empty = count == 0;
    match kind {
        "value_count" => json!({"value": count}),
        "cardinality" => {
            let mut seen: Vec<u64> = values.iter().map(|v| v.to_bits()).collect();
            seen.sort_unstable();
            seen.dedup();
            json!({"value": seen.len()})
        }
        "sum" => json!({"value": sum}),
        "avg" => json!({"value": avg}),
        "min" => json!({"value": (!empty).then_some(min)}),
        "max" => json!({"value": (!empty).then_some(max)}),
        "stats" => json!({
            "count": count,
            "min": (!empty).then_some(min),
            "max": (!empty).then_some(max),
            "avg": avg,
            "sum": sum,
        }),
        "extended_stats" => {
            let mean = avg.unwrap_or(0.0);
            let variance = if empty {
                None
            } else {
                Some(values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / count as f64)
            };
            json!({
                "count": count,
                "min": (!empty).then_some(min),
                "max": (!empty).then_some(max),
                "avg": avg,
                "sum": sum,
                "sum_of_squares": values.iter().map(|v| v * v).sum::<f64>(),
                "variance": variance,
                "variance_population": variance,
                "variance_sampling": (count > 1).then(|| {
                    values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (count - 1) as f64
                }),
                "std_deviation": variance.map(f64::sqrt),
                "std_deviation_population": variance.map(f64::sqrt),
                "std_deviation_sampling": (count > 1).then(|| {
                    (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (count - 1) as f64)
                        .sqrt()
                }),
                "std_deviation_bounds": {
                    "upper": variance.map(|v| mean + 2.0 * v.sqrt()),
                    "lower": variance.map(|v| mean - 2.0 * v.sqrt()),
                    "upper_population": variance.map(|v| mean + 2.0 * v.sqrt()),
                    "lower_population": variance.map(|v| mean - 2.0 * v.sqrt()),
                    "upper_sampling": variance.map(|v| mean + 2.0 * v.sqrt()),
                    "lower_sampling": variance.map(|v| mean - 2.0 * v.sqrt()),
                },
            })
        }
        _ => json!({"value": avg}),
    }
}

/// `scripted_metric`: four scripts and a `state` they share -- one to set
/// the state up, one run over each document, one to fold the state down,
/// and one over the folded states of every shard.
pub(crate) fn run_scripted_metric_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    use crate::painless::contexts::{Compiled, Runner};
    let spec = def.get("scripted_metric").cloned().unwrap_or(json!({}));
    let index = targets.first().cloned().unwrap_or_default();
    let params = spec.get("params").cloned().unwrap_or_else(|| json!({}));
    let compile = |key: &str| -> std::result::Result<Option<Compiled>, Response> {
        let Some(script) = spec.get(key) else { return Ok(None) };
        let mut script = script.clone();
        // the aggregation's own params are every script's params
        if let Some(o) = script.as_object_mut()
            && !o.contains_key("params")
        {
            o.insert("params".into(), params.clone());
        }
        if script.is_string() {
            script = json!({"source": script, "params": params});
        }
        Compiled::of(&script, &|id| store.stored_script(id))
            .map(Some)
            .map_err(|e| crate::search::search_script_failure(e, &index))
    };
    let init = compile("init_script")?;
    let map = compile("map_script")?;
    let combine = compile("combine_script")?;
    let reduce = compile("reduce_script")?;
    let Some(map) = map else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "[map_script] must be provided",
        ));
    };
    let failed = |e: crate::painless::ScriptError| crate::search::search_script_failure(e, &index);
    let state = crate::painless::Value::map(Vec::new());
    if let Some(init) = &init {
        Runner::new(&init.params).with_state(state.clone()).run(&init.script).map_err(failed)?;
    }
    let query = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
    let probe = json!({"query": query, "size": 10_000, "track_scores": true});
    let found = crate::search::run(store, &targets.join(","), &probe, &Params::new())?;
    for hit in &found.hits {
        let name = hit.get("_index").and_then(|v| v.as_str()).unwrap_or("");
        let Some(st) = store.get(name) else { continue };
        let mapping = st.read().mapping.clone();
        let source = hit.get("_source").cloned().unwrap_or(json!({}));
        let expanded = crate::store::expand_for_indexing(source, &mapping);
        let score = hit.get("_score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        Runner::new(&map.params)
            .with_state(state.clone())
            .with_doc(&expanded, &mapping)
            .with_score(score)
            .run(&map.script)
            .map_err(failed)?;
    }
    let combined = match &combine {
        Some(c) => {
            Runner::new(&c.params).with_state(state.clone()).run(&c.script).map_err(failed)?
        }
        None => state,
    };
    let value = match &reduce {
        Some(r) => Runner::new(&r.params)
            .with_states(crate::painless::Value::list(vec![combined]))
            .run(&r.script)
            .map_err(failed)?,
        None => crate::painless::Value::list(vec![combined]),
    };
    Ok(json!({"value": value.to_json()}))
}
