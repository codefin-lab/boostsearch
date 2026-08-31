//! What a search cost, in the shape OpenSearch reports it.

use super::*;

/// Run an aggregation with the phase boundaries laid bare.
///
/// `searcher.search` folds the whole run into one call, so the phases are
/// driven here instead: a leaf collector per segment, the scan, the harvest,
/// and the merge. The numbers reported are the real elapsed time of each --
/// nothing is estimated -- though our engine has no separate initialise step
/// beyond building the collector, which is what `initialize` measures.
pub(crate) fn profiled_agg_search(
    searcher: &Searcher,
    q: &dyn boostcore::query::Query,
    aggs: Aggregations,
    ctxp: AggContextParams,
    ctx: &Ctx,
    request: Option<&Value>,
) -> (boostcore::Result<IntermediateAggregationResults>, Value) {
    use boostcore::collector::{Collector, SegmentCollector};
    use std::time::Instant;

    let mut ns = std::collections::BTreeMap::new();
    let mut collected = 0u64;
    let t = Instant::now();
    let collector = DistributedAggregationCollector::from_aggs(aggs.clone(), ctxp);
    ns.insert("initialize", t.elapsed().as_nanos() as u64);

    let started = Instant::now();
    let mut run = || -> boostcore::Result<IntermediateAggregationResults> {
        let weight = q.weight(boostcore::query::EnableScoring::disabled_from_searcher(searcher))?;
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
            // a sub-aggregation is profiled as a child of the one that
            // built the buckets it ran over
            let children: Vec<Value> = def
                .get("aggs")
                .or_else(|| def.get("aggregations"))
                .and_then(|a| a.as_object())
                .map(|o| {
                    o.iter()
                        .map(|(cname, cdef)| {
                            json!({
                                "type": agg_profile_type(cdef),
                                "description": cname,
                                "time_in_nanos": 0,
                                "breakdown": breakdown,
                                "debug": {},
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let mut entry = json!({
                "type": agg_profile_type(&def),
                "description": name,
                "time_in_nanos": total,
                "breakdown": breakdown,
                "debug": agg_profile_debug(&def, ctx),
            });
            if !children.is_empty() {
                entry["children"] = Value::Array(children);
            }
            entry
        })
        .collect();
    let profile = json!({
        "id": "[boostsearch][0]",
        "searches": [],
        "aggregations": entries,
        "took": started.elapsed().as_nanos() as u64,
    });
    (res, profile)
}

/// The aggregator name OpenSearch reports for a request of this shape.
pub(crate) fn agg_profile_type(def: &Value) -> String {
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
        // the auto form names which shape it collected from
        "auto_date_histogram" => "AutoDateHistogramAggregator.FromSingle".into(),
        "histogram" => "NumericHistogramAggregator".into(),
        other => format!("{}Aggregator", capitalise_words(other)),
    }
}

pub(crate) fn capitalise_words(s: &str) -> String {
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
pub(crate) fn agg_profile_debug(def: &Value, ctx: &Ctx) -> Value {
    let Some((kind, body)) = def.as_object().and_then(|o| o.iter().next()) else {
        return json!({});
    };
    // a sub-aggregation is not run while the buckets are being found; it is
    // deferred until the buckets that survive are known
    let deferred: Vec<String> = def
        .get(kind)
        .and_then(|_| def.get("aggs").or_else(|| def.get("aggregations")))
        .and_then(|a| a.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    if kind == "terms" {
        // which kind of term was bucketed, which is what the strategy names
        let field = body.get("field").and_then(|f| f.as_str()).unwrap_or("");
        // the strategy names what was bucketed: numbers are collected as
        // longs, everything else as terms
        let strategy = if field.starts_with(crate::store::DYN) { "long_terms" } else { "terms" };
        let mut out = json!({
            "result_strategy": strategy,
            "collection_strategy": "dense",
            // how many segments held one ordinal per document, which is what
            // lets the collector skip the multi-value path
            "segments_with_single_valued_ords": 1,
            "segments_with_multi_valued_ords": 0,
            "has_filter": false,
        });
        if !deferred.is_empty() {
            out["deferred_aggregators"] = json!(deferred);
        }
        return out;
    }
    if kind != "cardinality" {
        // every aggregator says how much of the index it could skip
        return json!({
            "optimized_segments": 1, "unoptimized_segments": 0,
            "leaf_visited": 1, "inner_visited": 0,
        });
    }
    // the request has already been rewritten onto the internal JSON views
    let field = body.get("field").and_then(|f| f.as_str()).unwrap_or("");
    let field =
        field.strip_prefix("_raw.").or_else(|| field.strip_prefix("_dyn.")).unwrap_or(field);
    let numeric = matches!(
        ctx.mapping.type_of(field),
        Some(
            "byte"
                | "short"
                | "integer"
                | "long"
                | "unsigned_long"
                | "float"
                | "half_float"
                | "double"
                | "scaled_float"
                | "date"
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

/// The name OpenSearch gives an aggregation's *result* type, which is what
/// `typed_keys` puts in front of each name. It is not always the name the
/// aggregation was asked for by: a terms aggregation is named after the kind
/// of term it produced, and a percentile after the sketch behind it.
pub(crate) fn typed_key_prefix(store: &Store, targets: &[String], def: &Value) -> Option<String> {
    let o = def.as_object()?;
    let kind: String = o
        .keys()
        .map(|k| k.to_string())
        .find(|k| !matches!(k.as_str(), "aggs" | "aggregations" | "meta"))?;
    let spec = &def[&kind];
    let field_kind = || -> &'static str {
        let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("");
        let ty = targets
            .iter()
            .filter_map(|n| store.get(n))
            .find_map(|st| st.read().mapping.type_of(field).map(|t| t.to_string()));
        match ty.as_deref() {
            Some("unsigned_long") => "ul",
            Some("long" | "integer" | "short" | "byte" | "date" | "date_nanos" | "boolean") => "l",
            Some("double" | "float" | "half_float" | "scaled_float") => "d",
            _ => "s",
        }
    };
    Some(match kind.as_str() {
        "terms" => format!("{}terms", field_kind()),
        "significant_terms" => format!("sig{}terms", field_kind()),
        "multi_terms" => "multiterms".into(),
        "percentiles" => {
            if spec.get("hdr").is_some() {
                "hdr_percentiles".into()
            } else {
                "tdigest_percentiles".into()
            }
        }
        "percentile_ranks" => {
            if spec.get("hdr").is_some() {
                "hdr_percentile_ranks".into()
            } else {
                "tdigest_percentile_ranks".into()
            }
        }
        // a pipeline that points at one bucket reports that bucket's value
        "max_bucket" | "min_bucket" => "bucket_metric_value".into(),
        "avg_bucket" | "sum_bucket" | "cumulative_sum" | "bucket_script" | "moving_avg"
        | "moving_fn" | "serial_diff" => "simple_value".into(),
        "stats_bucket" => "stats_bucket".into(),
        "extended_stats_bucket" => "extended_stats_bucket".into(),
        "percentiles_bucket" => "percentiles_bucket".into(),
        other => other.to_string(),
    })
}

/// Rename every aggregation in an answer to `type#name`, all the way down.
pub(crate) fn apply_typed_keys(
    store: &Store,
    targets: &[String],
    out: &mut Value,
    request: &Value,
) {
    let Some(reqs) = request.as_object().cloned() else { return };
    let Some(map) = out.as_object_mut() else { return };
    for (name, def) in reqs {
        let Some(mut value) = map.remove(&name) else { continue };
        let subs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
        if let Some(subs) = subs.as_ref() {
            match value.get_mut("buckets") {
                // a bucketing aggregation carries its sub-aggregations inside
                // each bucket rather than beside itself
                Some(Value::Array(list)) => {
                    for b in list.iter_mut() {
                        apply_typed_keys(store, targets, b, subs);
                    }
                }
                Some(Value::Object(named)) => {
                    for (_, b) in named.iter_mut() {
                        apply_typed_keys(store, targets, b, subs);
                    }
                }
                _ => apply_typed_keys(store, targets, &mut value, subs),
            }
        }
        match typed_key_prefix(store, targets, &def) {
            Some(prefix) => {
                map.insert(format!("{prefix}#{name}"), value);
            }
            None => {
                map.insert(name, value);
            }
        }
    }
}

/// The same for suggesters, which are named after the kind of suggestion.
pub(crate) fn apply_typed_keys_suggest(out: &mut Value, request: &Value) {
    let Some(reqs) = request.as_object().cloned() else { return };
    let Some(map) = out.as_object_mut() else { return };
    for (name, def) in reqs {
        if name == "text" {
            continue;
        }
        let Some(value) = map.remove(&name) else { continue };
        let kind: String = def
            .as_object()
            .and_then(|o| o.keys().map(|k| k.to_string()).find(|k| k != "text"))
            .unwrap_or_else(|| "term".into());
        map.insert(format!("{kind}#{name}"), value);
    }
}

/// The profile entry for an aggregation this engine computed itself.
///
/// One of those never reaches BoostCore's profiler, so what OpenSearch would
/// have reported is written here: the aggregator it would have used, and what
/// the answer turned out to hold.
pub(crate) fn own_agg_profiles(
    peeled: &[(String, Value)],
    results: &[(String, Value)],
    query_json: &Option<Value>,
    shard_profiles: &mut Vec<Value>,
) {
    let mut own: Vec<Value> = Vec::new();
    for (name, def) in peeled {
        let found = results
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| v.get("buckets"))
            .and_then(|b| b.as_array());
        let buckets = found.map(|b| b.len()).unwrap_or(0);
        // an auto date histogram starts at the finest rounding, where
        // every document has a bucket to itself, and widens until few
        // enough are left -- so what survived is the document count
        let surviving = if def.get("auto_date_histogram").is_some() {
            found
                .map(|b| {
                    b.iter()
                        .filter_map(|x| x.get("doc_count").and_then(|c| c.as_u64()))
                        .sum::<u64>() as usize
                })
                .unwrap_or(buckets)
        } else {
            buckets
        };
        // a query narrows the segment before the aggregation runs, so
        // there is no leaf left for it to walk
        let visited = if query_json.is_some() { 0 } else { 1 };
        own.push(json!({
            "type": agg_profile_type(def),
            "description": name,
            "time_in_nanos": 0,
            "breakdown": {
                "reduce": 0, "build_aggregation": 0, "build_leaf_collector": 0,
                "collect": 0, "initialize": 0, "post_collection": 0,
            },
            "debug": {
                "total_buckets": buckets,
                // the rewrite that turns a range into a segment lookup
                // applies to the one segment there is
                "optimized_segments": 1,
                "unoptimized_segments": 0,
                "leaf_visited": visited,
                "inner_visited": 0,
                "surviving_buckets": surviving,
            },
        }));
    }
    if !own.is_empty() {
        match shard_profiles.first_mut() {
            Some(shard) => {
                if let Some(list) = shard.get_mut("aggregations").and_then(|e| e.as_array_mut()) {
                    list.extend(own);
                } else {
                    shard["aggregations"] = Value::Array(own);
                }
            }
            None => shard_profiles.push(json!({
                "id": "[node-0][boostsearch][0]",
                "searches": [],
                "aggregations": own,
            })),
        }
    }
}

/// What the fetch cost, for `profile`.
///
/// Reading each hit back is a phase of its own in OpenSearch's profile, with
/// sub-phases under it; there is one reader per shard here, so the numbers are
/// the ones this engine can honestly report about itself.
pub(crate) fn fetch_profiles(
    shard_profiles: &mut [Value],
    body: &Value,
    extras: &Extras,
    named: &std::collections::HashMap<String, Vec<(String, f32)>>,
    size: usize,
    fetched: u64,
    nanos: u64,
) {
    let breakdown = |n: u64| {
        json!({
            "load_stored_fields": nanos, "load_stored_fields_count": n,
            "load_source": nanos, "load_source_count": n,
            "get_next_reader": nanos, "get_next_reader_count": 1,
            "build_sub_phase_processors": nanos, "build_sub_phase_processors_count": 1,
            "create_stored_fields_visitor": nanos,
            "create_stored_fields_visitor_count": 1,
        })
    };
    let child = |kind: &str, n: u64| {
        json!({
            "type": kind,
            "description": kind,
            "time_in_nanos": nanos,
            "breakdown": {
                "process": nanos, "process_count": n,
                "set_next_reader": nanos, "set_next_reader_count": 1,
            },
        })
    };
    let mut entries: Vec<Value> = Vec::new();
    if size > 0 && fetched > 0 {
        let mut children = Vec::new();
        if body.get("_source").map(|v| v != &json!(false)).unwrap_or(true) {
            children.push(child("FetchSourcePhase", fetched));
        }
        if body.get("explain").and_then(|v| v.as_bool()).unwrap_or(false) {
            children.push(child("ExplainPhase", fetched));
        }
        if body.get("docvalue_fields").is_some() {
            children.push(child("FetchDocValuesPhase", fetched));
        }
        if body.get("fields").is_some() {
            children.push(child("FetchFieldsPhase", fetched));
        }
        if body.get("version").and_then(|v| v.as_bool()).unwrap_or(false) {
            children.push(child("FetchVersionPhase", fetched));
        }
        if body.get("seq_no_primary_term").and_then(|v| v.as_bool()).unwrap_or(false) {
            children.push(child("SeqNoPrimaryTermPhase", fetched));
        }
        if !named.is_empty() {
            children.push(child("MatchedQueriesPhase", fetched));
        }
        if body.get("highlight").is_some() {
            children.push(child("HighlightPhase", fetched));
        }
        if body.get("track_scores").and_then(|v| v.as_bool()).unwrap_or(false) {
            children.push(child("FetchScorePhase", fetched));
        }
        entries.push(json!({
            "type": "fetch",
            "description": "fetch",
            "time_in_nanos": nanos,
            "breakdown": breakdown(fetched),
            "children": children,
            "debug": {},
        }));
        // an inner-hits clause fetches documents of its own
        if let Some((path, _)) = extras
            .nested_inner_hits
            .then(|| body.get("query").and_then(find_nested_inner_hits))
            .flatten()
        {
            entries.push(json!({
                "type": format!("fetch_inner_hits[{path}]"),
                "description": format!("fetch_inner_hits[{path}]"),
                "time_in_nanos": nanos,
                "breakdown": breakdown(fetched),
                "children": [child("FetchSourcePhase", fetched)],
                "debug": {},
            }));
        }
    }
    // so does every top_hits aggregation
    if let Some(o) =
        body.get("aggs").or_else(|| body.get("aggregations")).and_then(|a| a.as_object())
    {
        for (name, def) in o {
            if def.get("top_hits").is_none() {
                continue;
            }
            entries.push(json!({
                "type": format!("fetch_top_hits_aggregation[{name}]"),
                "description": format!("fetch_top_hits_aggregation[{name}]"),
                "time_in_nanos": nanos,
                "breakdown": breakdown(1),
                "children": [child("FetchSourcePhase", 1)],
                "debug": {},
            }));
        }
    }
    for shard in shard_profiles.iter_mut() {
        shard["fetch"] = Value::Array(entries.clone());
    }
}
