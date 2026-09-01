//! Who answers which aggregation, and what has to happen to the request
//! before BoostCore is given it.

use super::*;
use crate::search::*;

mod check;
pub(crate) use check::*;
mod rewrite;
pub(crate) use rewrite::*;

/// Whether the total can be had without walking the matches.
///
/// `Weight::count` reads the figure from the postings header for a term query
/// and from the segment for a match-all, and otherwise counts by iterating.
/// Splitting top-k from the count only pays where that shortcut exists: where
/// it does not, the count walks everything the pruned pass just avoided, and
/// two passes beat one only in the wrong direction.
pub(crate) fn count_without_walking(query_json: &Option<Value>) -> bool {
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
            .map(|o| o.len() == 1 && o.values().next().map(|v| !v.is_object()).unwrap_or(false))
            .unwrap_or(false),
        _ => false,
    }
}

/// How many documents the query matches.
///
/// `Weight::count` reads it straight from the postings header where the query
/// allows -- a term query with no deletions knows its own document frequency --
/// and falls back to walking the matches where it does not.
pub(crate) fn count_matches(
    searcher: &Searcher,
    query: &dyn boostcore::query::Query,
) -> boostcore::Result<usize> {
    let weight = query.weight(boostcore::query::EnableScoring::disabled_from_searcher(searcher))?;
    let mut total = 0usize;
    for reader in searcher.segment_readers() {
        total += weight.count(reader)? as usize;
    }
    Ok(total)
}

/// Turn what the shards collected into the answer a client reads.
///
/// The shards hand back intermediate results; combining them is BoostCore's
/// job, and everything after that is this engine's: the shapes OpenSearch
/// writes a bucket key in, the orders and partitions taken off the request
/// before it was parsed, and the `meta` a caller attached.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finalise_aggs(
    store: &Store,
    targets: &[String],
    acc: Option<IntermediateAggregationResults>,
    req: Option<Aggregations>,
    agg_json: &Option<Value>,
    bucket_orders: &[(String, String, bool)],
    partitions: &[(String, i64, i64, usize)],
    agg_meta: &[(String, Value)],
    weighted: bool,
) -> std::result::Result<Option<Value>, Response> {
    let out = match (acc, req) {
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
                            st.read()
                                .mapping
                                .types
                                .iter()
                                .map(|(k, t)| (k.clone(), t.clone()))
                                .collect::<Vec<_>>()
                        })
                        .collect();
                    date_histogram_keys(&mut v, req, &types);
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
                apply_bucket_orders(&mut v, bucket_orders);
                apply_partitions(&mut v, partitions);
                reattach_meta(&mut v, agg_meta);
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
    Ok(out)
}

/// Run the aggregations BoostCore could not parse, each as its own search.
pub(crate) fn run_peeled_aggs(
    store: &Store,
    targets: &[String],
    query_json: &Option<Value>,
    peeled: &[(String, Value)],
    weighted: bool,
) -> std::result::Result<Vec<(String, Value)>, Response> {
    let mut out: Vec<(String, Value)> = Vec::new();
    for (name, def) in peeled {
        // what the request attached to the aggregation travels with its answer
        let own_meta = def.get("meta").cloned();
        let mut v = run_peeled_agg(store, targets, query_json, name, def, weighted)?;
        if let Some(m) = own_meta {
            v["meta"] = m;
        }
        out.push((name.clone(), v));
    }
    Ok(out)
}

pub(crate) fn plan_aggs(
    store: &Store,
    targets: &[String],
    body: &Value,
) -> std::result::Result<AggPlan, Response> {
    let mut agg_json = body.get("aggs").or_else(|| body.get("aggregations")).cloned();
    if agg_json.as_ref().map(composite_under_a_parent).unwrap_or(false) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "[composite] aggregation cannot be used with a parent aggregation of type: [terms]",
        ));
    }
    // Parameter bounds do not depend on any mapping, so they are checked here
    // rather than per shard: a request that names no existing index has no
    // shards to walk, and a bad parameter would otherwise pass unread.
    if let Some(a) = agg_json.as_ref() {
        check_agg_bounds(a, "")?;
    }
    // buckets have to be weighted only where a document stands for several
    let weighted = targets.iter().filter_map(|n| store.get(n)).any(|st| st.read().has_doc_count);
    if weighted && let Some(a) = agg_json.as_mut() {
        inject_doc_count_helpers(a);
    }
    if let Some(a) = agg_json.as_mut() {
        // a filter aggregation can carry a terms lookup too
        resolve_terms_lookups(store, a)?;
    }
    // BoostCore has `filter` but not `filters`; peel those out and run them
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
    // a pipeline that sits *inside* a bucketing aggregation reads that
    // aggregation's own buckets, so it is taken out of the request and applied
    // to the answer once the buckets are there
    let mut bucket_pipelines: Vec<(Vec<String>, String, Value)> = Vec::new();
    if let Some(node) = agg_json.as_mut() {
        strip_bucket_pipelines(node, &mut Vec::new(), &mut bucket_pipelines);
    }
    // one of those written at the top level has no buckets to read
    if let Some((_, name, def)) = bucket_pipelines.iter().find(|(at, _, _)| at.is_empty()) {
        let kind = def
            .as_object()
            .and_then(|o| {
                o.keys()
                    .map(|k| k.to_string())
                    .find(|k| BUCKET_PIPELINES.contains(&k.as_str()) || k == "bucket_sort")
            })
            .unwrap_or_default();
        // the ones that read a series want a histogram over it; the rest only
        // want somewhere to sit
        let want = match kind.starts_with("bucket_") {
            true => "must be declared inside of another aggregation".to_string(),
            false => "must have a histogram, date_histogram or auto_date_histogram as parent \
                      but doesn't have a parent"
                .to_string(),
        };
        return Err(err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            format!("Validation Failed: 1: {kind} aggregation [{name}] {want};"),
        ));
    }
    let mut filters_aggs: Vec<(String, Value)> = Vec::new();
    if let Some(Value::Object(o)) = agg_json.as_mut() {
        let names: Vec<String> = o
            .iter()
            .filter(|(_, def)| {
                // anything under it that has to be run here drags the whole
                // aggregation out of BoostCore's hands with it
                peelable(def)
                    || def.get("filters").is_some()
                    || def.get("missing").is_some()
                    || def.get("median_absolute_deviation").is_some()
                    // percentiles answer a different question from BoostCore's
                    // sketch, which is approximate where OpenSearch's is exact
                    // over the handful of values these aggregations see
                    || def.get("percentiles").is_some()
                    // `_index` is metadata, not a column: bucket it ourselves
                    || def.get("global").is_some()
                    || def
                        .get("terms")
                        .and_then(|t| t.get("field"))
                        .and_then(|f| f.as_str())
                        == Some("_index")
                    // BoostCore's own `filter` agg only speaks its query-string
                    // dialect, so run singular filters through our query builder
                    || def.get("filter").is_some()
                    || def.get("composite").is_some()
                    || def.get("multi_terms").is_some()
                    || def.get("rare_terms").is_some()
                    || def.get("nested").is_some()
                    || def.get("reverse_nested").is_some()
                    || def.get("sampler").is_some()
                    || def.get("children").is_some()
                    || def.get("parent").is_some()
                    || def.get("geo_bounds").is_some()
                    || def.get("geo_centroid").is_some()
                    || def.get("matrix_stats").is_some()
                    || def.get("diversified_sampler").is_some()
                    || def.get("geo_distance").is_some()
                    || def.get("percentile_ranks").is_some()
                    || def.get("significant_terms").is_some()
                    || def.get("significant_text").is_some()
                    || def.get("ip_range").is_some()
                    || def.get("date_range").is_some()
                    || def.get("adjacency_matrix").is_some()
                    || def.get("weighted_avg").is_some()
                    || def.get("auto_date_histogram").is_some()
                    || def.get("variable_width_histogram").is_some()
                    // calendar units are not fixed lengths, and a named zone is
                    // a history of offsets; a fixed step over the numbers the
                    // index holds is a plain histogram, which BoostCore runs
                    || def.get("date_histogram").map(walked_here).unwrap_or(false)
                    // a range field holds no single value to bucket a document
                    // by, so BoostCore's histogram sees nothing there at all
                    || def
                        .get("histogram")
                        .and_then(|h| h.get("field"))
                        .and_then(|f| f.as_str())
                        .map(|f| range_field(store, targets, f))
                        .unwrap_or(false)
                    // a field no document has, standing in for every document
                    || def
                        .get("terms")
                        .and_then(|t| t.get("field"))
                        .and_then(|f| f.as_str())
                        .map(|f| {
                            def.pointer("/terms/missing").is_some()
                                && unmapped_field(store, targets, f)
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
    Ok(AggPlan {
        request: agg_json,
        peeled: filters_aggs,
        siblings: pipeline_aggs,
        inner: bucket_pipelines,
        weighted,
    })
}

pub(crate) fn combine(main: &Option<Value>, extra: Option<Value>) -> Value {
    match (main, extra) {
        (Some(m), Some(e)) => json!({"bool": {"must": [m.clone()], "filter": [e]}}),
        (Some(m), None) => m.clone(),
        (None, Some(e)) => e,
        (None, None) => json!({"match_all": {}}),
    }
}

/// Split sub-aggregations into the ones this engine computes itself and the
/// ones BoostCore can parse, so each set can take the path that suits it.
pub(crate) fn split_peelable(sub_aggs: &Option<Value>) -> (Option<Value>, Option<Value>) {
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

/// Is this aggregation, or anything under it, one that has to be computed a
/// bucket at a time here?
pub(crate) fn peelable(def: &Value) -> bool {
    peelable_here(def)
        || def
            .get("aggs")
            .or_else(|| def.get("aggregations"))
            .and_then(|s| s.as_object())
            .map(|o| o.values().any(peelable))
            .unwrap_or(false)
}

/// Is this an aggregation BoostCore has no parser for, which has to be computed
/// a bucket at a time here instead?
pub(crate) fn peelable_here(def: &Value) -> bool {
    const OWN: &[&str] = &[
        "missing",
        "median_absolute_deviation",
        "filter",
        "global",
        "weighted_avg",
        "variable_width_histogram",
        "auto_date_histogram",
        "date_range",
        "ip_range",
        "adjacency_matrix",
        "rare_terms",
        "multi_terms",
        "composite",
        "significant_terms",
        "significant_text",
        "top_hits",
        "nested",
        "reverse_nested",
        "geo_distance",
        "percentile_ranks",
        "children",
        "parent",
        "geo_bounds",
        "geo_centroid",
        "matrix_stats",
        "sampler",
        "diversified_sampler",
    ];
    OWN.iter().any(|k| def.get(k).is_some())
        || def.get("date_histogram").map(walked_here).unwrap_or(false)
}

/// A date histogram this engine has to walk itself, a bucket at a time: one
/// stepping by a calendar unit, one reported in a zone that is not simply UTC,
/// or one over a field whose numbers are not the milliseconds a key is in.
pub(crate) fn walked_here(spec: &Value) -> bool {
    if spec.get("calendar_interval").is_some() {
        return true;
    }
    if fixed_step_ms(spec).is_none() {
        return true;
    }
    // any zone but UTC has to be placed here: even one that is on UTC today
    // may not have been at the instant a bucket falls in
    match spec.get("time_zone").and_then(|v| v.as_str()).map(|z| z.trim()) {
        None | Some("") => false,
        Some(z) => !matches!(z, "Z" | "UTC" | "utc" | "+00:00" | "-00:00" | "+0000" | "-0000"),
    }
}

/// The step a date histogram takes, in milliseconds, when it is a fixed length.
pub(crate) fn fixed_step_ms(spec: &Value) -> Option<i64> {
    spec.get("fixed_interval")
        .or_else(|| spec.get("interval"))
        .and_then(|v| v.as_str())
        .and_then(parse_offset)
        .map(|d| d.whole_milliseconds() as i64)
        .filter(|ms| *ms > 0)
}

/// Turn a fixed-step date histogram into the histogram it is.
///
/// A date is milliseconds in the index, so a step of so many milliseconds over
/// that column is the same bucketing -- and BoostCore walks it in one pass
/// instead of this engine counting each bucket with its own query.
pub(crate) fn fixed_date_histograms(node: &mut Value, ctx: &Ctx) {
    let Some(map) = node.as_object_mut() else { return };
    for (_, def) in map.iter_mut() {
        let Some(d) = def.as_object_mut() else { continue };
        if let Some(sub) = d.get_mut("aggs") {
            fixed_date_histograms(sub, ctx);
        }
        let Some(spec) = d.get("date_histogram").cloned() else { continue };
        if walked_here(&spec) {
            continue;
        }
        let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
        // a date_nanos counts in nanoseconds, and a key is milliseconds
        if ctx.mapping.type_of(&field) != Some("date") {
            continue;
        }
        let Some(step) = fixed_step_ms(&spec) else { continue };
        let offset = spec
            .get("offset")
            .and_then(|v| v.as_str())
            .and_then(parse_offset)
            .map(|o| o.whole_milliseconds() as i64)
            .unwrap_or(0)
            .rem_euclid(step);
        let mut hist = json!({"field": field, "interval": step, "offset": offset});
        if let Some(min) = spec.get("min_doc_count") {
            hist["min_doc_count"] = min.clone();
        }
        for key in ["hard_bounds", "extended_bounds"] {
            let Some(b) = spec.get(key) else { continue };
            let edge = |name: &str| -> Option<i64> {
                crate::store::date_number(b.get(name)?, None, false)
            };
            if let (Some(min), Some(max)) = (edge("min"), edge("max")) {
                hist[key] = json!({"min": min, "max": max});
            }
        }
        d.remove("date_histogram");
        d.insert("histogram".into(), hist);
    }
}

/// Count the documents a query matches, and run its sub-aggregations --
/// including the ones BoostCore cannot parse, which are run here against the
/// same query rather than handed down.
pub(crate) fn count_with_sub_aggs(
    store: &Store,
    targets: &[String],
    query_json: &Value,
    sub_aggs: &Option<Value>,
    weighted: bool,
) -> std::result::Result<(u64, Option<Value>), Response> {
    let Some(subs) = sub_aggs.as_ref().and_then(|s| s.as_object()) else {
        return filtered_count(store, targets, query_json, sub_aggs);
    };
    let (mine, theirs): (Vec<_>, Vec<_>) = subs.iter().partition(|(_, d)| peelable(d));
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

pub(crate) fn filtered_count(
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
        let searcher = g.reader.searcher();
        total += searcher.search(&q, &Count).map_err(|e| {
            err(StatusCode::BAD_REQUEST, "search_phase_execution_exception", e.to_string())
        })? as u64;

        if let Some(sa) = sub_aggs {
            let mut rewritten = sa.clone();
            let mut ignored = Vec::new();
            normalize_aggs(&mut rewritten, &mut ignored, false);
            rewrite_agg_fields(&mut rewritten, &ctx);
            let parsed: Aggregations = serde_json::from_value(rewritten)
                .map_err(|e| err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string()))?;
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
        (Some(a), Some(r)) => a
            .into_final_result(r, Default::default())
            .ok()
            .and_then(|v| serde_json::to_value(v).ok()),
        _ => None,
    };
    Ok((total, sub))
}
