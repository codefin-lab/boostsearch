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
    } else if def.pointer("/terms/script").is_some() {
        run_scripted_terms_agg(store, targets, query_json, def, weighted)
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
    } else if def.get("geohash_grid").is_some() {
        crate::search::run_geo_grid_agg(store, targets, query_json, def, "geohash_grid")
    } else if def.get("geotile_grid").is_some() {
        crate::search::run_geo_grid_agg(store, targets, query_json, def, "geotile_grid")
    } else if def.get("geo_bounds").is_some() {
        crate::search::run_geo_bounds_agg(store, targets, query_json, def)
    } else if def.get("geo_centroid").is_some() {
        crate::search::run_geo_centroid_agg(store, targets, query_json, def)
    } else if def.get("matrix_stats").is_some() {
        run_matrix_stats_agg(store, targets, query_json, def)
    } else if def.get("geo_distance").is_some() {
        run_geo_distance_agg(store, targets, query_json, def, weighted)
    } else if def.get("percentile_ranks").is_some() {
        run_percentile_ranks(store, targets, query_json, def)
    } else if def.get("children").is_some() || def.get("parent").is_some() {
        run_join_agg(store, targets, query_json, def, weighted)
    } else if def.get("sampler").is_some() || def.get("diversified_sampler").is_some() {
        run_sampler_agg(store, targets, query_json, def, weighted)
    } else if def.get("nested").is_some() || def.get("reverse_nested").is_some() {
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

/// `matrix_stats` -- how a set of numeric fields move, together and apart.
/// The one number a field's values stand for.
fn reduce_values(held: &Value, mode: &str, narrow: bool) -> Option<f64> {
    let kept = |v: f64| match narrow {
        true => v as f32 as f64,
        false => v,
    };
    let numbers: Vec<f64> = match held {
        Value::Array(items) => items.iter().filter_map(|v| v.as_f64()).map(kept).collect(),
        other => return other.as_f64().map(kept),
    };
    if numbers.is_empty() {
        return None;
    }
    Some(match mode {
        "min" => numbers.iter().cloned().fold(f64::INFINITY, f64::min),
        "max" => numbers.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        "sum" => numbers.iter().sum(),
        "median" => {
            let mut sorted = numbers.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let middle = sorted.len() / 2;
            match sorted.len() % 2 {
                0 => (sorted[middle - 1] + sorted[middle]) / 2.0,
                _ => sorted[middle],
            }
        }
        _ => numbers.iter().sum::<f64>() / numbers.len() as f64,
    })
}

pub(crate) fn run_matrix_stats_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("matrix_stats").cloned().unwrap_or(json!({}));
    let fields: Vec<String> = spec
        .get("fields")
        .and_then(|f| f.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    // a matrix is worked out over the values a document holds, which a script
    // does not stand for
    if spec.get("script").is_some() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "parsing_exception",
            "[matrix_stats] unknown field [script]",
        ));
    }
    // where a field holds several values, one of them stands for the document
    let mode = spec.get("mode").and_then(|v| v.as_str()).unwrap_or("avg").to_string();
    // a document with no value for a field may be counted as holding one
    let missing = spec.get("missing").cloned().unwrap_or_else(|| json!({}));
    let probe = json!({
        "query": main_query.clone().unwrap_or_else(|| json!({"match_all": {}})),
        "size": 10_000,
        "_source": fields.clone(),
    });
    let answer = run(store, &targets.join(","), &probe, &Params::new())?;
    // only the documents that hold every field count, which is what makes a
    // covariance a covariance
    // which of the fields are kept at single precision: the ones no mapping
    // widened to a double
    let narrow: std::collections::HashSet<String> = fields
        .iter()
        .filter(|field| {
            targets.iter().all(|name| {
                store
                    .get(name)
                    .map(|st| {
                        !matches!(
                            st.read().mapping.type_of(field),
                            Some("double" | "long" | "integer" | "short" | "byte" | "scaled_float")
                        )
                    })
                    .unwrap_or(true)
            })
        })
        .cloned()
        .collect();
    let mut shards: Vec<usize> = Vec::new();
    let rows: Vec<Vec<f64>> = answer
        .hits
        .iter()
        .filter_map(|hit| {
            // which shard held this document, which is where its moments were
            // worked out
            let index = hit.get("_index").and_then(|v| v.as_str()).unwrap_or_default();
            let id = hit.get("_id").and_then(|v| v.as_str()).unwrap_or_default();
            let over = store.get(index).map(|st| st.read().shard_count().max(1)).unwrap_or(1);
            let shard = crate::search::routing_shard(id, over) as usize;
            fields
                .iter()
                .map(|field| {
                    let held = hit.pointer(&format!("/_source/{}", field.replace('.', "/")));
                    // a field nobody mapped holds a floating point number as
                    // a `float`, which is the width each of its values is kept
                    // at before they are reduced to one
                    let width = narrow.contains(field);
                    match held {
                        Some(v) => reduce_values(v, &mode, width),
                        None => missing.get(field).and_then(|v| v.as_f64()),
                    }
                })
                .collect::<Option<Vec<f64>>>()
                .inspect(|_| shards.push(shard))
        })
        .collect();
    let count = rows.len();
    if count == 0 {
        return Ok(json!({"doc_count": 0}));
    }
    // each shard works out its own moments and the shard results are merged,
    // which is the order the arithmetic happens in on a real cluster and the
    // order the numbers are pinned to
    let width = fields.len();
    let mut per_shard: Vec<crate::search::Running> = Vec::new();
    for (shard, row) in shards.iter().zip(rows.iter()) {
        while per_shard.len() <= *shard {
            per_shard.push(crate::search::Running::new(width));
        }
        per_shard[*shard].add(row);
    }
    let mut running = crate::search::Running::new(width);
    for shard in &per_shard {
        running.merge(shard);
    }
    let described = running.described();
    let count = described.count;
    let mut fields_out = Vec::new();
    for (at, field) in fields.iter().enumerate() {
        let with: serde_json::Map<String, Value> = fields
            .iter()
            .enumerate()
            .map(|(other, name)| (name.clone(), json!(described.covariances[at][other])))
            .collect();
        let correlation: serde_json::Map<String, Value> = fields
            .iter()
            .enumerate()
            .map(|(other, name)| {
                let r = match at == other {
                    true => 1.0,
                    false => {
                        described.covariances[at][other]
                            / (described.variances[at].sqrt() * described.variances[other].sqrt())
                    }
                };
                (name.clone(), json!(r))
            })
            .collect();
        fields_out.push(json!({
            "name": field,
            "count": count,
            "mean": described.means[at],
            "variance": described.variances[at],
            "skewness": described.skewness[at],
            "kurtosis": described.kurtosis[at],
            "covariance": with,
            "correlation": correlation,
        }));
    }
    // the fields are named back in the order OpenSearch names them
    fields_out.sort_by(|a, b| {
        b.get("name").and_then(|v| v.as_str()).cmp(&a.get("name").and_then(|v| v.as_str()))
    });
    Ok(json!({"doc_count": count, "fields": fields_out}))
}

/// `sampler` -- the best few documents rather than all of them.
///
/// A sampler narrows what its sub-aggregations see to the documents the query
/// scored highest, which is how a significant-terms aggregation is kept from
/// reading the whole index. `diversified_sampler` narrows it further: at most
/// so many documents for each value of a field, so that one crowded value
/// cannot fill the sample.
pub(crate) fn run_sampler_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
    weighted: bool,
) -> std::result::Result<Value, Response> {
    let diversified = def.get("diversified_sampler");
    let spec = diversified.or_else(|| def.get("sampler")).cloned().unwrap_or(json!({}));
    let most = spec.get("shard_size").and_then(|v| v.as_u64()).unwrap_or(100) as usize;
    let per_value = spec.get("max_docs_per_value").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
    let field = spec.get("field").and_then(|v| v.as_str()).map(|s| s.to_string());
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();

    let mut probe = json!({
        "query": main_query.clone().unwrap_or_else(|| json!({"match_all": {}})),
        // more than the sample keeps: the ones a crowded value pushes out have
        // to come from somewhere
        "size": (most.max(1) * per_value.max(1)).saturating_mul(10).min(10_000),
        "_source": false,
    });
    if let Some(field) = field.as_deref() {
        probe["_source"] = json!([field]);
    }
    let found = run(store, &targets.join(","), &probe, &Params::new())?;
    let mut kept: Vec<String> = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for hit in &found.hits {
        let Some(id) = hit.get("_id").and_then(|v| v.as_str()) else { continue };
        if let (Some(field), true) = (field.as_deref(), diversified.is_some()) {
            let value = hit
                .pointer(&format!("/_source/{}", field.replace('.', "/")))
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            let count = seen.entry(value).or_insert(0);
            if *count >= per_value {
                continue;
            }
            *count += 1;
        }
        kept.push(id.to_string());
        if kept.len() >= most {
            break;
        }
    }
    // what the sample holds is what the sub-aggregations are asked about
    let narrowed = json!({"ids": {"values": kept}});
    let (count, subs) = count_with_sub_aggs(store, targets, &narrowed, &sub_aggs, weighted)?;
    let mut out = json!({ "doc_count": count });
    if let Some(Value::Object(map)) = subs {
        for (name, value) in map {
            out[name] = value;
        }
    }
    Ok(out)
}

/// `children` and `parent` -- the documents on the other side of a join.
///
/// A `children` aggregation aggregates over the children of the documents its
/// bucket holds, and `parent` over their parents. Documents are stored whole
/// here, so each is a query for the other side and then the ordinary walk.
pub(crate) fn run_join_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
    weighted: bool,
) -> std::result::Result<Value, Response> {
    let children = def.get("children");
    let spec = children.or_else(|| def.get("parent")).cloned().unwrap_or(json!({}));
    let named = spec.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let here = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
    // an index with no join field has no other side to look at
    let Some(field) = crate::search::join_field(store, targets) else {
        let mut out = json!({"doc_count": 0});
        if let Some(Value::Object(map)) =
            count_with_sub_aggs(store, targets, &json!({"match_none": {}}), &sub_aggs, weighted)?.1
        {
            for (name, value) in map {
                out[name] = value;
            }
        }
        return Ok(out);
    };
    // whichever side the aggregation names, the other side is what its bucket
    // is asked about
    let narrowed = match children.is_some() {
        // the documents of that kind whose parent the query found
        true => {
            let parents = crate::search::matching_ids_here(store, targets, &here);
            json!({"bool": {"must": [
                {"term": {format!("{field}.name"): named}},
                {"terms": {format!("{field}.parent"): parents}},
            ]}})
        }
        // the parents of the documents of that kind the query found
        false => {
            let of_that_kind = json!({
                "bool": {"must": [here.clone(), {"term": {format!("{field}.name"): named}}]}
            });
            let parents = crate::search::ids_of_field(
                store,
                targets,
                &of_that_kind,
                &format!("{field}.parent"),
            );
            json!({"ids": {"values": parents}})
        }
    };
    let (count, subs) = count_with_sub_aggs(store, targets, &narrowed, &sub_aggs, weighted)?;
    let mut out = json!({ "doc_count": count });
    if let Some(Value::Object(map)) = subs {
        for (name, value) in map {
            out[name] = value;
        }
    }
    Ok(out)
}
