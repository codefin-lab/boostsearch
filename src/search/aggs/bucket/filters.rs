//! The aggregations that bucket by a query rather than by a value.

use super::*;

/// Run one `filters` aggregation: a separate filtered search per bucket, with
/// the bucket's sub-aggregations evaluated inside it.
pub(crate) fn run_filters_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("filters").cloned().unwrap_or(json!({}));
    let inner = spec.get("filters").cloned().unwrap_or(Value::Null);
    let other_bucket_key =
        spec.get("other_bucket_key").and_then(|v| v.as_str()).map(|s| s.to_string());
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
        if let Some(sub) = sub
            && let Some(o) = sub.as_object()
        {
            for (k, v) in o {
                b[k] = v.clone();
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
        if let Some(sub) = sub
            && let Some(o) = sub.as_object()
        {
            for (k, v) in o {
                b[k] = v.clone();
            }
        }
        buckets.push((Some(other_bucket_key.unwrap_or_else(|| "_other_".into())), b));
    }

    // keyed only answers when every bucket has a name to be keyed by
    let named: Option<Vec<(String, Value)>> =
        buckets.iter().cloned().map(|(n, b)| n.map(|n| (n, b))).collect();
    if let Some(named) = named {
        Ok(json!({"buckets": Value::Object(named.into_iter().collect())}))
    } else {
        Ok(json!({"buckets": buckets.into_iter().map(|(_, b)| b).collect::<Vec<_>>()}))
    }
}

/// Run one aggregation that BoostCore cannot parse itself.
///
/// These are computed by asking a question per bucket rather than by walking
/// the documents once, so any of them may appear inside any other: what a
/// bucket narrows to is just another query to ask the next one with.
pub(crate) fn run_peeled_agg(
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
        run_filter_agg(
            store,
            targets,
            &None,
            &json!({
                "filter": {"match_all": {}},
                "aggs": def.get("aggs").or_else(|| def.get("aggregations")).cloned()
                    .unwrap_or_else(|| json!({}))
            }),
        )
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
        .map(|f| def.pointer("/terms/missing").is_some() && unmapped_field(store, targets, f))
        .unwrap_or(false)
    {
        run_missing_terms_agg(store, targets, query_json, def)
    } else if def.get("histogram").is_some() {
        run_range_field_histogram(store, targets, query_json, def)
    } else if def.get("ip_range").is_some() {
        run_ip_range_agg(store, targets, query_json, def)
    } else if def.get("adjacency_matrix").is_some() {
        run_adjacency_matrix_agg(store, targets, query_json, def)
    } else if def
        .get("terms")
        .map(|t| t.get("field").and_then(|f| f.as_str()) != Some("_index"))
        .unwrap_or(false)
    {
        run_field_terms_agg(store, targets, query_json, def, weighted)
    } else if def.get("geo_distance").is_some() {
        run_geo_distance_agg(store, targets, query_json, def, weighted)
    } else if def.get("percentile_ranks").is_some() {
        run_percentile_ranks(store, targets, query_json, def)
    } else if def.get("nested").is_some()
        || def.get("reverse_nested").is_some()
        || def.get("sampler").is_some()
        || def.get("diversified_sampler").is_some()
    {
        // documents are stored whole here, so the objects a nested aggregation
        // would descend into are already part of the document it is under
        {
            let path = def.pointer("/nested/path").and_then(|p| p.as_str()).map(|s| s.to_string());
            // what sits under a nested aggregation works on the objects at that
            // path, and only the hits it may ask for come from the documents
            let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
            let asks_for_hits =
                sub_aggs.as_ref().map(|s| s.to_string().contains("top_hits")).unwrap_or(false);
            if let (Some(path), false, true) = (path.as_deref(), asks_for_hits, sub_aggs.is_some())
            {
                return run_nested_over_objects(store, targets, query_json, path, &sub_aggs);
            }
            let mut answer = run_filter_agg(store, targets, query_json, &{
                let mut d = def.clone();
                // inside a nested aggregation the sorting is done in that
                // object's scope, which is what the aggregation stands for
                if let Some(path) = path.as_deref() {
                    scope_sorts_to(&mut d, path);
                }
                if let Some(o) = d.as_object_mut() {
                    o.remove("nested");
                    o.remove("reverse_nested");
                    o.remove("sampler");
                    o.remove("diversified_sampler");
                    o.insert("filter".into(), json!({"match_all": {}}));
                }
                d
            })?;
            // inside a nested aggregation the documents are the objects at
            // that path, so any hits reported under it are those objects
            if let Some(path) = path {
                expand_nested_hits(&mut answer, &path);
            }
            Ok(answer)
        }
    } else if def.get("top_hits").is_some() {
        run_top_hits(store, targets, query_json, def)
    } else if def.get("significant_terms").is_some() || def.get("significant_text").is_some() {
        run_significant_terms(store, targets, query_json, def, name)
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

/// `missing`: a single bucket of documents that have no value for the field.
pub(crate) fn run_missing_agg(
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
    if let Some(sub) = sub
        && let Some(o) = sub.as_object()
    {
        for (k, v) in o {
            out[k] = v.clone();
        }
    }
    Ok(out)
}

/// `filter`: one bucket holding the documents that match a sub-query.
pub(crate) fn run_filter_agg(
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
    if let Some(sub) = sub
        && let Some(o) = sub.as_object()
    {
        for (k, v) in o {
            out[k] = v.clone();
        }
    }
    Ok(out)
}

/// `adjacency_matrix`: how the named filters overlap.
///
/// One bucket per filter, and one per pair of filters for the documents both
/// select. Pairs that select nothing are left out.
pub(crate) fn run_adjacency_matrix_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("adjacency_matrix").cloned().unwrap_or(json!({}));
    let separator = spec.get("separator").and_then(|v| v.as_str()).unwrap_or("&").to_string();
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let Some(filters) = spec.get("filters").and_then(|f| f.as_object()) else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "[filters] cannot be empty",
        ));
    };
    let named: Vec<(String, Value)> = filters.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

    let mut buckets = Vec::new();
    let push = |key: String,
                filter: Value,
                buckets: &mut Vec<Value>|
     -> std::result::Result<(), Response> {
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
