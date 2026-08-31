//! The aggregations that make buckets this engine fills itself.

use super::*;
use crate::search::*;

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

/// `terms` over the `_index` metadata field: one bucket per index that has hits.
/// `terms` over an ordinary field, run here because something under it has to
/// be: BoostCore still finds the buckets, and each one then narrows the query
/// for the aggregations it could not parse.
pub(crate) fn run_field_terms_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
    weighted: bool,
) -> std::result::Result<Value, Response> {
    let spec = def.get("terms").cloned().unwrap_or(json!({}));
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let (peeled_subs, plain_subs) = split_peelable(&sub_aggs);
    // an order naming a sub-aggregation that is run here cannot be asked of
    // BoostCore, which will not have that aggregation; the buckets are put in
    // order once it has answered
    let order = spec.get("order").cloned();
    let ordered_here = order
        .as_ref()
        .and_then(|o| o.as_object())
        .and_then(|o| o.keys().next().cloned())
        .map(|k| {
            let head = k.split('.').next().unwrap_or("").to_string();
            peeled_subs
                .as_ref()
                .and_then(|p| p.as_object())
                .map(|p| p.contains_key(&head))
                .unwrap_or(false)
        })
        .unwrap_or(false);
    let mut spec = spec;
    if ordered_here && let Some(o) = spec.as_object_mut() {
        o.remove("order");
    }
    let mut node = json!({"terms": spec});
    if let Some(plain) = plain_subs.as_ref() {
        node["aggs"] = plain.clone();
    }
    let request = json!({"__f": node});
    let query = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
    let (_, res) = filtered_count(store, targets, &query, &Some(request))?;
    let Some(res) = res else { return Ok(json!({"buckets": []})) };
    let mut answer = res.get("__f").cloned().unwrap_or_else(|| json!({"buckets": []}));
    // the order a terms aggregation is asked for by default is most documents
    // first, and between two of the same size the smaller key
    if order.is_none()
        && let Some(buckets) = answer.get_mut("buckets").and_then(|b| b.as_array_mut())
    {
        buckets.sort_by(|a, b| {
            let count = |v: &Value| v.get("doc_count").and_then(|c| c.as_u64()).unwrap_or(0);
            let key = |v: &Value| match v.get("key") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            count(b).cmp(&count(a)).then_with(|| key(a).cmp(&key(b)))
        });
    }
    if let Some(peeled) = peeled_subs.as_ref().and_then(|v| v.as_object()) {
        let empty = Vec::new();
        let buckets = answer["buckets"].as_array().cloned().unwrap_or(empty);
        let mut filled = Vec::new();
        for mut b in buckets {
            let key = b.get("key").cloned().unwrap_or(Value::Null);
            let mut filters = vec![json!({"term": {field.clone(): key}})];
            if let Some(q) = main_query.as_ref() {
                filters.push(q.clone());
            }
            let narrowed = Some(json!({"bool": {"filter": filters}}));
            for (n, d) in peeled {
                b[n.clone()] = run_peeled_agg(store, targets, &narrowed, n, d, weighted)?;
            }
            filled.push(b);
        }
        if ordered_here
            && let Some((key, dir)) = order
                .as_ref()
                .and_then(|o| o.as_object())
                .and_then(|o| o.iter().next())
                .map(|(k, v)| (k.clone(), v.as_str() == Some("desc")))
        {
            filled.sort_by(|a, b| {
                let ord = compare_bucket_by(a, b, &key);
                if dir { ord.reverse() } else { ord }
            });
        }
        answer["buckets"] = Value::Array(filled);
    }
    Ok(answer)
}

pub(crate) fn run_index_terms_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let size = def.get("terms").and_then(|t| t.get("size")).and_then(|v| v.as_u64()).unwrap_or(10)
        as usize;
    let min_doc_count =
        def.get("terms").and_then(|t| t.get("min_doc_count")).and_then(|v| v.as_u64()).unwrap_or(1);
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

/// `date_range`: one bucket per span of time.
///
/// Each range becomes a filter on the field, so the ordinary query path
/// answers it. The bounds are reported in epoch milliseconds however they were
/// written, while the key keeps the caller's own spelling.
pub(crate) fn run_date_range_agg(
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
    let mapped_format = targets.iter().filter_map(|n| store.get(n)).next().and_then(|st| {
        st.read()
            .mapping
            .field_option(&field, "format")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
    });
    let format =
        spec.get("format").and_then(|f| f.as_str()).map(|s| s.to_string()).or(mapped_format);

    // a bound is the number the index holds, and the date it stands for
    let millis = |v: &Value| crate::store::date_number(v, format.as_deref(), false);
    let iso = |v: &Value| {
        millis(v).and_then(|ms| crate::store::format_millis(ms, "strict_date_optional_time"))
    };
    // a bound is named in the key the way it is reported beside it, not the
    // way the request happened to spell it
    let shown = |v: &Option<Value>| match v {
        // a bound written as a date is named in the key the way it is
        // reported beside it; one written as a number is a number
        Some(Value::String(s)) => iso(&json!(s)).unwrap_or_else(|| s.clone()),
        Some(other) if !other.is_null() => other.to_string(),
        _ => "*".to_string(),
    };

    let mut buckets = Vec::new();
    let mut keyed_out = serde_json::Map::new();
    // AbstractRangeBuilder sorts the ranges it was given by where they start,
    // so the buckets come back in that order however the request listed them
    let mut asked: Vec<Value> =
        spec.get("ranges").and_then(|r| r.as_array()).cloned().unwrap_or_default();
    let edge = |range: &Value, key: &str, open: f64| -> f64 {
        range.get(key).filter(|v| !v.is_null()).and_then(millis).map(|ms| ms as f64).unwrap_or(open)
    };
    asked.sort_by(|a, b| {
        edge(a, "from", f64::NEG_INFINITY)
            .total_cmp(&edge(b, "from", f64::NEG_INFINITY))
            .then_with(|| edge(a, "to", f64::INFINITY).total_cmp(&edge(b, "to", f64::INFINITY)))
    });
    for range in &asked {
        let from = range.get("from").cloned().filter(|v| !v.is_null());
        let to = range.get("to").cloned().filter(|v| !v.is_null());
        let mut clause = serde_json::Map::new();
        if let Some(f) = from.as_ref().and_then(millis) {
            clause.insert("gte".into(), json!(f));
        }
        if let Some(t) = to.as_ref().and_then(millis) {
            clause.insert("lt".into(), json!(t));
        }
        // the bounds are already the numbers the index holds, whatever format
        // the field itself was written in
        if !clause.is_empty() {
            clause.insert("format".into(), json!("epoch_millis"));
        }
        let unbounded = clause.is_empty();
        // a document with no value stands in with what `missing` names, and
        // so belongs to whichever bucket that value falls in
        let missing_here = missing
            .as_ref()
            .and_then(millis)
            .map(|ms| {
                from.as_ref().and_then(millis).map(|f| ms >= f).unwrap_or(true)
                    && to.as_ref().and_then(millis).map(|t| ms < t).unwrap_or(true)
            })
            .unwrap_or(false);
        let filter = if unbounded {
            // documents with no value take part when a stand-in was named
            if missing.is_some() {
                json!({"match_all": {}})
            } else {
                json!({"exists": {"field": field}})
            }
        } else if missing_here {
            json!({"bool": {"should": [
                {"range": {field.clone(): Value::Object(clause)}},
                {"bool": {"must_not": [{"exists": {"field": field}}]}},
            ], "minimum_should_match": 1}})
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
        if let Some(f) = from.as_ref()
            && let Some(ms) = millis(f)
        {
            b["from"] = json!(ms);
            if let Some(s) = iso(f) {
                b["from_as_string"] = json!(s);
            }
        }
        if let Some(t) = to.as_ref()
            && let Some(ms) = millis(t)
        {
            b["to"] = json!(ms);
            if let Some(s) = iso(t) {
                b["to_as_string"] = json!(s);
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

/// A terms aggregation over a field no document has.
///
/// With `missing` given, every document takes that one value, so there is one
/// bucket holding all of them. `value_type` says how the key is written --
/// what the field would have been, had anything ever put a value in it.
pub(crate) fn run_missing_terms_agg(
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

/// A numeric histogram over a range field.
///
/// A range document has no single value to fall into one bucket; it covers a
/// span, and belongs to every bucket that span touches. So each bucket is
/// counted on its own, by asking which stored ranges overlap it, rather than
/// by reading a column of values the field does not have.
pub(crate) fn run_range_field_histogram(
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
            let read =
                |k: &str| -> Option<f64> { extremes.as_ref()?.get(k)?.get("value")?.as_f64() };
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
        if let (Some(sub), Some(o)) = (sub, b.as_object_mut())
            && let Some(entries) = sub.as_object()
        {
            for (k, v) in entries {
                o.insert(k.clone(), v.clone());
            }
        }
        buckets.push(b);
    }
    Ok(json!({"buckets": buckets}))
}

pub(crate) fn run_ip_range_agg(
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

/// `rare_terms`: the terms few documents carry.
///
/// It is `terms` read from the other end -- keep the buckets at or below
/// `max_doc_count` instead of the largest ones -- so it is answered by
/// collecting the buckets and filtering, ordered by key.
pub(crate) fn run_rare_terms_agg(
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
    let (_, res) = filtered_count(store, targets, &query, &Some(request.clone()))?;
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
            include.as_ref().map(matches).unwrap_or(true)
                && !exclude.as_ref().map(matches).unwrap_or(false)
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

/// `multi_terms`: one bucket per combination of several fields' values.
///
/// The combinations come from nesting a `terms` aggregation per field and
/// flattening the tree, the same way `composite` builds its keys. What differs
/// is the answer: a key is the list of values rather than a named object, and
/// buckets are ranked by how many documents they hold rather than by key.
pub(crate) fn run_multi_terms_agg(
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

    // an aggregation BoostCore cannot parse cannot ride down with the terms
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
    let (_, res) = filtered_count(store, targets, &query, &Some(request.clone()))?;
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

    let mut total: u64 = flat.iter().map(|(_, c, _)| *c).sum();
    // Each shard answers with its own top few and says how much it left out;
    // the counts that come back are therefore the shards' own, not the whole
    // index's. That only shows where there is more than one shard and a
    // `shard_size` small enough to cut something off.
    let shards = targets
        .iter()
        .filter_map(|n| store.get(n))
        .map(|st| st.read().shard_count())
        .max()
        .unwrap_or(1);
    let asked_shard_size = spec.get("shard_size").and_then(|v| v.as_u64());
    if let Some(shard_size) = asked_shard_size.filter(|_| shards > 1) {
        let shard_size = shard_size.max(1) as usize;
        let probe = json!({
            "query": query.clone(),
            "size": 10_000,
            "_source": fields.clone(),
        });
        if let Ok(answer) = run(store, &targets.join(","), &probe, &Params::new()) {
            let mut per_shard: std::collections::HashMap<u64, Vec<Vec<Value>>> =
                std::collections::HashMap::new();
            for hit in &answer.hits {
                let Some(id) = hit.get("_id").and_then(|v| v.as_str()) else { continue };
                let mut combos: Vec<Vec<Value>> = vec![Vec::new()];
                let mut usable = true;
                for (i, field) in fields.iter().enumerate() {
                    let at = hit.pointer(&format!("/_source/{}", field.replace('.', "/")));
                    let values: Vec<Value> = match at {
                        Some(Value::Array(a)) => a.clone(),
                        Some(other) => vec![other.clone()],
                        None => match missings.get(i).and_then(|m| m.clone()) {
                            Some(m) => vec![m],
                            None => {
                                usable = false;
                                break;
                            }
                        },
                    };
                    combos = combos
                        .into_iter()
                        .flat_map(|c| {
                            values.iter().map(move |v| {
                                let mut c = c.clone();
                                c.push(v.clone());
                                c
                            })
                        })
                        .collect();
                }
                if !usable {
                    continue;
                }
                // a document written with a routing value is placed by that
                // value rather than by its id
                let placed_by = targets
                    .iter()
                    .filter_map(|n| store.get(n))
                    .find_map(|st| st.read().routing.get(id).cloned())
                    .unwrap_or_else(|| id.to_string());
                per_shard.entry(routing_shard(&placed_by, shards)).or_default().extend(combos);
            }
            let mut merged: std::collections::HashMap<String, (Vec<Value>, u64)> =
                std::collections::HashMap::new();
            let mut other = 0u64;
            for (_, combos) in per_shard {
                let mut counts: std::collections::HashMap<String, (Vec<Value>, u64)> =
                    std::collections::HashMap::new();
                for c in combos {
                    let key = format!("{c:?}");
                    counts.entry(key).or_insert((c, 0)).1 += 1;
                }
                let here: u64 = counts.values().map(|(_, n)| *n).sum();
                let mut ranked: Vec<(String, (Vec<Value>, u64))> = counts.into_iter().collect();
                ranked.sort_by(|a, b| b.1.1.cmp(&a.1.1).then_with(|| a.0.cmp(&b.0)));
                ranked.truncate(shard_size);
                let kept: u64 = ranked.iter().map(|(_, (_, n))| *n).sum();
                other += here - kept;
                for (key, (combo, n)) in ranked {
                    let slot = merged.entry(key).or_insert((combo, 0));
                    slot.1 += n;
                }
            }
            flat = merged.into_values().map(|(key, n)| (key, n, serde_json::Map::new())).collect();
            total = flat.iter().map(|(_, c, _)| *c).sum::<u64>() + other;
        }
    }
    // `min_doc_count: 0` asks for combinations the index holds but the query
    // did not match, so the key space comes from the whole index
    if min_doc_count == 0 && main_query.is_some() {
        let (_, all) = filtered_count(store, targets, &json!({"match_all": {}}), &Some(request))?;
        if let Some(all) = all {
            let mut every = Vec::new();
            flatten_multi_terms(&all, 0, fields.len(), &mut Vec::new(), &mut every);
            let mut seen: std::collections::HashSet<String> =
                flat.iter().map(|(k, _, _)| format!("{k:?}")).collect();
            for (key, _, _) in every {
                if seen.insert(format!("{key:?}")) {
                    flat.push((key, 0, serde_json::Map::new()));
                }
            }
        }
    }
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
        let k =
            |v: &Value| v.get("key_as_string").and_then(|s| s.as_str()).unwrap_or("").to_string();
        k(a).cmp(&k(b))
    });
    let kept: u64 = buckets
        .iter()
        .take(size)
        .map(|b| b.get("doc_count").and_then(|d| d.as_u64()).unwrap_or(0))
        .sum();
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
pub(crate) fn parse_multi_terms_order(order: Option<&Value>) -> Vec<(String, bool)> {
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
pub(crate) fn compare_bucket_by(a: &Value, b: &Value, key: &str) -> Ordering {
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
pub(crate) fn multi_terms_key(v: Value, ty: Option<&str>) -> Value {
    match ty {
        Some("ip") => {
            v.as_str().and_then(crate::store::ip_from_canonical).map(Value::String).unwrap_or(v)
        }
        Some("boolean") => match v.as_u64() {
            Some(n) => Value::Bool(n != 0),
            None => v,
        },
        // a date key is shown as a date, from the number the index holds
        Some(ty @ ("date" | "date_nanos")) => {
            let millis = v.as_f64().map(|n| if ty == "date_nanos" { n / 1e6 } else { n });
            millis
                .and_then(|ms| crate::store::format_millis(ms as i64, "strict_date_optional_time"))
                .map(Value::String)
                .unwrap_or(v)
        }
        _ => v,
    }
}

pub(crate) fn flatten_multi_terms(
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
                    if k != "key"
                        && k != "doc_count"
                        && k != "key_as_string"
                        && !k.starts_with("__m")
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

/// `variable_width_histogram`: buckets whose edges follow the data.
///
/// The values are sorted and cut at the widest gaps, which puts the boundaries
/// where the data is already sparse. Each bucket is keyed by the mean of what
/// it holds.
pub(crate) fn run_variable_width_histogram(
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
    let mut cuts: Vec<usize> =
        gaps.into_iter().take(want.saturating_sub(1)).map(|(_, i)| i).collect();
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
pub(crate) fn run_auto_date_histogram(
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
    // a date is a number in the index: milliseconds, or nanoseconds for a
    // date_nanos
    let per_ns: f64 = field
        .as_str()
        .and_then(|f| {
            targets.iter().filter_map(|n| store.get(n)).find_map(|st| {
                match st.read().mapping.type_of(f) {
                    Some("date_nanos") => Some(1.0),
                    Some(t) if t.starts_with("date") => Some(1_000_000.0),
                    _ => None,
                }
            })
        })
        .unwrap_or(1.0);
    let span_ns = ((hi - lo) * per_ns).max(0.0);

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

/// `significant_terms`: the terms that stand out in what the query matched,
/// compared with how common they are in the index as a whole.
///
/// The measure is the one OpenSearch calls JLH: how much more of the
/// foreground a term takes up than of the background, multiplied by the ratio
/// between the two, so that a term has to be both commoner *and* markedly
/// commoner to score well.
pub(crate) fn run_significant_terms(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
    asked_as: &str,
) -> std::result::Result<Value, Response> {
    let kind = if def.get("significant_text").is_some() {
        "significant_text"
    } else {
        "significant_terms"
    };
    let name = def
        .as_object()
        .and_then(|o| o.keys().map(|k| k.to_string()).find(|k| k == kind))
        .unwrap_or_else(|| kind.to_string());
    let spec = def.get(&name).cloned().unwrap_or(json!({}));
    // the measures this aggregation can score by, and the options it takes
    const KNOWN: &[&str] = &[
        "field",
        "script",
        "size",
        "shard_size",
        "min_doc_count",
        "shard_min_doc_count",
        "include",
        "exclude",
        "execution_hint",
        "background_filter",
        "filter_duplicate_text",
        "source_fields",
        "jlh",
        "mutual_information",
        "chi_square",
        "gnd",
        "percentage",
        "script_heuristic",
    ];
    if let Some(stray) = spec
        .as_object()
        .and_then(|o| o.keys().map(|k| k.to_string()).find(|k| !KNOWN.contains(&k.as_str())))
    {
        let near = KNOWN.iter().find(|k| edit_distance(k, &stray) <= 2).copied().unwrap_or("jlh");
        return Err(err(
            StatusCode::BAD_REQUEST,
            "parsing_exception",
            format!("[{name}] unknown field [{stray}] did you mean [{near}]?"),
        ));
    }
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    let size = spec.get("size").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
    // a term seen once or twice is noise, so the floor is higher here than it
    // is for a plain terms aggregation
    let min_doc_count = spec.get("min_doc_count").and_then(|v| v.as_u64()).unwrap_or(3);

    let string_field = targets
        .iter()
        .filter_map(|n| store.get(n))
        .find_map(|st| st.read().mapping.type_of(&field).map(|t| t.to_string()))
        .map(|t| matches!(t.as_str(), "text" | "keyword" | "match_only_text"))
        .unwrap_or(true);
    for key in ["include", "exclude"] {
        if spec.get(key).map(|v| v.is_string()).unwrap_or(false) && !string_field {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!(
                    "Aggregation [{asked_as}] cannot support regular expression style \
                     include/exclude settings as they can only be applied to string fields. Use \
                     an array of values for include/exclude clauses"
                ),
            ));
        }
    }
    let listed = |key: &str| -> Option<Vec<String>> {
        let a = spec.get(key)?.as_array()?;
        Some(a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
    };
    let include = listed("include");
    let exclude = listed("exclude");

    let query = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
    // A significant *text* aggregation asks about the words in a field rather
    // than about its value. Words are not kept in a column -- only in the term
    // index -- so the text is read back and analysed again, which is what
    // OpenSearch does for this aggregation too.
    let analysed = kind == "significant_text"
        || targets
            .iter()
            .filter_map(|n| store.get(n))
            .find_map(|st| st.read().mapping.type_of(&field).map(|t| t.to_string()))
            .map(|t| matches!(t.as_str(), "text" | "match_only_text"))
            .unwrap_or(false);
    let counted = |q: &Value| -> std::result::Result<(u64, Vec<(Value, u64)>), Response> {
        if !analysed {
            let request =
                json!({"__s": {"terms": {"field": field, "size": 65_536, "min_doc_count": 1}}});
            let (total, res) = filtered_count(store, targets, q, &Some(request))?;
            let buckets = res
                .as_ref()
                .and_then(|r| r.pointer("/__s/buckets"))
                .and_then(|b| b.as_array())
                .map(|a| {
                    a.iter()
                        .map(|b| {
                            (
                                b.get("key").cloned().unwrap_or(Value::Null),
                                b.get("doc_count").and_then(|c| c.as_u64()).unwrap_or(0),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            return Ok((total, buckets));
        }
        let probe = json!({"query": q.clone(), "size": 10_000, "_source": [field.clone()]});
        let answer = run(store, &targets.join(","), &probe, &Params::new())?;
        let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        // `filter_duplicate_text` is for text that repeats itself across
        // documents -- boilerplate, signatures, quoted replies. A run of words
        // already seen in an earlier document says nothing new about this one,
        // so the words in it are passed over.
        let dedup = spec.get("filter_duplicate_text").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut seen_runs: std::collections::HashSet<String> = std::collections::HashSet::new();
        const RUN: usize = 3;
        let total = answer.hits.len() as u64;
        for hit in &answer.hits {
            let Some(text) = hit.pointer(&format!("/_source/{}", field.replace('.', "/"))) else {
                continue;
            };
            let text = match text {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            // a word counts once for the document it is in, however often it
            // is repeated there
            let mut tokens: Vec<String> = Vec::new();
            for st in targets.iter().filter_map(|n| store.get(n)).take(1) {
                let g = st.read();
                tokens = crate::query::analyze_text(&g.index, &text, None);
            }
            let mut skip = vec![false; tokens.len()];
            if dedup && tokens.len() >= RUN {
                let mut fresh = Vec::new();
                for i in 0..=tokens.len() - RUN {
                    let run = tokens[i..i + RUN].join(" ");
                    if seen_runs.contains(&run) {
                        for slot in skip.iter_mut().skip(i).take(RUN) {
                            *slot = true;
                        }
                    } else {
                        fresh.push(run);
                    }
                }
                for run in fresh {
                    seen_runs.insert(run);
                }
            }
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for (i, token) in tokens.into_iter().enumerate() {
                if skip.get(i).copied().unwrap_or(false) {
                    continue;
                }
                seen.insert(token);
            }
            for token in seen {
                *counts.entry(token).or_insert(0) += 1;
            }
        }
        let mut out: Vec<(Value, u64)> = counts.into_iter().map(|(k, c)| (json!(k), c)).collect();
        out.sort_by_key(|a| std::cmp::Reverse(a.1));
        Ok((total, out))
    };
    let (fg_total, fg) = counted(&query)?;
    let (bg_total, bg) = counted(&json!({"match_all": {}}))?;
    let read = |res: &Vec<(Value, u64)>| -> Vec<(Value, u64)> { res.clone() };
    let background: std::collections::HashMap<String, u64> =
        read(&bg).into_iter().map(|(k, c)| (k.to_string(), c)).collect();

    let ip_field = targets
        .iter()
        .filter_map(|n| store.get(n))
        .any(|st| st.read().mapping.type_of(&field) == Some("ip"));
    let mut buckets: Vec<Value> = Vec::new();
    for (key, count) in read(&fg) {
        if count < min_doc_count {
            continue;
        }
        let bg_count = background.get(&key.to_string()).copied().unwrap_or(count);
        let key = if ip_field {
            key.as_str().and_then(crate::store::ip_from_canonical).map(Value::String).unwrap_or(key)
        } else {
            key
        };
        let text = match &key {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if let Some(only) = include.as_ref()
            && !only.contains(&text)
        {
            continue;
        }
        if let Some(never) = exclude.as_ref()
            && never.contains(&text)
        {
            continue;
        }
        let fg_pct = count as f64 / fg_total.max(1) as f64;
        let bg_pct = bg_count as f64 / bg_total.max(1) as f64;
        let score = if bg_pct > 0.0 { (fg_pct - bg_pct) * (fg_pct / bg_pct) } else { 0.0 };
        buckets.push(json!({
            "key": key,
            "doc_count": count,
            "score": score,
            "bg_count": bg_count,
        }));
    }
    buckets.sort_by(|a, b| {
        let s = |v: &Value| v.get("score").and_then(|x| x.as_f64()).unwrap_or(0.0);
        s(b).partial_cmp(&s(a)).unwrap_or(Ordering::Equal).then_with(|| {
            let k = |v: &Value| v.get("key").map(|x| x.to_string()).unwrap_or_default();
            k(a).cmp(&k(b))
        })
    });
    buckets.truncate(size);
    Ok(json!({"doc_count": fg_total, "bg_count": bg_total, "buckets": buckets}))
}
