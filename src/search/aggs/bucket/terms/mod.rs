//! The aggregations that bucket by the values a field holds.

use super::*;

mod multi;
pub(crate) use multi::*;
mod significant;
pub(crate) use significant::*;

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
