//! The aggregations that bucket by the values a field holds.

use super::*;

mod multi;
pub(crate) use multi::*;
mod text;
pub(crate) use text::*;
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
    let (peeled_subs, plain_subs) = split_peelable(&sub_aggs, store, targets);
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

/// A terms aggregation whose keys a script makes: over each value of a
/// field (`_value`), or over the document itself (`doc`, `_score`).
///
/// BoostCore's engine reads a field; a script has to be run here, over the
/// documents the query finds.
pub(crate) fn run_scripted_terms_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
    weighted: bool,
) -> std::result::Result<Value, Response> {
    let spec = def.get("terms").cloned().unwrap_or_else(|| json!({}));
    let field = spec.get("field").and_then(|f| f.as_str()).map(|s| s.to_string());
    let script = spec.get("script").cloned().unwrap_or(Value::Null);
    let compiled = crate::painless::contexts::Compiled::of(&script, &|id| store.stored_script(id))
        .map_err(|e| {
            crate::search::search_script_failure(
                e,
                targets.first().map(|s| s.as_str()).unwrap_or(""),
            )
        })?;
    let query = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
    let probe = json!({"query": query, "size": 10_000, "track_scores": true});
    let found = crate::search::run(store, &targets.join(","), &probe, &Params::new())?;
    // what kind of key the field gives, so the buckets read as its values do
    let numeric = field.as_ref().and_then(|f| {
        targets.iter().find_map(|t| {
            let st = store.get(t)?;
            let g = st.read();
            g.mapping.type_of(f).map(|k| {
                matches!(
                    k,
                    "long"
                        | "integer"
                        | "short"
                        | "byte"
                        | "double"
                        | "float"
                        | "half_float"
                        | "scaled_float"
                        | "unsigned_long"
                        | "date"
                        | "boolean"
                )
            })
        })
    });
    let mut counts: Vec<(Value, u64, Vec<String>)> = Vec::new();
    let mut note = |key: Value, id: String| match counts.iter_mut().find(|(k, _, _)| *k == key) {
        Some(entry) => {
            entry.1 += 1;
            entry.2.push(id);
        }
        None => counts.push((key, 1, vec![id])),
    };
    for hit in &found.hits {
        let index = hit.get("_index").and_then(|v| v.as_str()).unwrap_or("");
        let id = hit.get("_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let score = hit.get("_score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let source = hit.get("_source").cloned().unwrap_or(json!({}));
        let Some(st) = store.get(index) else { continue };
        let mapping = st.read().mapping.clone();
        let expanded = crate::store::expand_for_indexing(source, &mapping);
        let mut keys: Vec<Value> = Vec::new();
        let run_with = |runner: crate::painless::contexts::Runner| -> Result<crate::painless::Value, Response> {
            let mut runner = runner;
            runner.run(&compiled.script).map_err(|e| crate::search::search_script_failure(e, index))
        };
        match &field {
            Some(f) => {
                // the script maps each value the field holds
                let held = expanded.pointer(&format!("/{}", f.replace('.', "/"))).cloned();
                let values: Vec<Value> = match held {
                    Some(Value::Array(a)) => a,
                    Some(Value::Null) | None => Vec::new(),
                    Some(other) => vec![other],
                };
                for v in values {
                    let typed = crate::painless::Value::from_json(&v);
                    let runner = crate::painless::contexts::Runner::new(&compiled.params)
                        .with_doc(&expanded, &mapping)
                        .with_score(score)
                        .with_value(typed);
                    let made = run_with(runner)?;
                    keys.push(scripted_key(&made, numeric, index)?);
                }
            }
            None => {
                let runner = crate::painless::contexts::Runner::new(&compiled.params)
                    .with_doc(&expanded, &mapping)
                    .with_score(score);
                let made = run_with(runner)?;
                match &made {
                    crate::painless::Value::List(l) => {
                        for v in l.borrow().iter() {
                            keys.push(scripted_key(v, numeric, index)?);
                        }
                    }
                    other => keys.push(scripted_key(other, numeric, index)?),
                }
            }
        }
        keys.dedup();
        for k in keys {
            note(k, id.clone());
        }
    }
    let order = spec.get("order").cloned();
    let (order_key, descending) = order
        .as_ref()
        .and_then(|o| o.as_object())
        .and_then(|o| o.iter().next())
        .map(|(k, v)| (k.clone(), v.as_str() == Some("desc")))
        .unwrap_or(("_count".into(), true));
    counts.sort_by(|a, b| {
        let by_key = || match (&a.0, &b.0) {
            (Value::Number(x), Value::Number(y)) => {
                x.as_f64().partial_cmp(&y.as_f64()).unwrap_or(std::cmp::Ordering::Equal)
            }
            (x, y) => x.to_string().cmp(&y.to_string()),
        };
        let ord = if order_key == "_key" {
            by_key()
        } else {
            a.1.cmp(&b.1).then_with(|| by_key().reverse())
        };
        if descending { ord.reverse() } else { ord }
    });
    let min_doc_count = spec.get("min_doc_count").and_then(|v| v.as_u64()).unwrap_or(1);
    let size = spec.get("size").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
    let total: u64 = counts.iter().map(|c| c.1).sum();
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let mut buckets = Vec::new();
    let mut shown: u64 = 0;
    for (key, count, ids) in counts.into_iter().filter(|c| c.1 >= min_doc_count).take(size) {
        shown += count;
        let mut b = json!({"key": key, "doc_count": count});
        if let Some(subs) = sub_aggs.as_ref().and_then(|s| s.as_object()) {
            let narrowed = Some(json!({"bool": {"filter": [
                {"ids": {"values": ids}},
                query.clone(),
            ]}}));
            for (n, d) in subs {
                b[n.clone()] = run_peeled_agg(store, targets, &narrowed, n, d, weighted)?;
            }
        }
        buckets.push(b);
    }
    Ok(json!({
        "doc_count_error_upper_bound": 0,
        "sum_other_doc_count": total - shown,
        "buckets": buckets,
    }))
}

/// A script's result as a bucket key: a number where the field is numeric,
/// else the text of it. A value that holds itself has no key.
fn scripted_key(
    v: &crate::painless::Value,
    numeric: Option<bool>,
    index: &str,
) -> std::result::Result<Value, Response> {
    use crate::painless::Value as PV;
    if v.try_json().is_err() {
        return Err(crate::search::search_shard_failure(
            "illegal_argument_exception",
            "Iterable object is self-referencing itself (ScriptBytesValues value)",
            index,
        ));
    }
    Ok(match (numeric, v) {
        (Some(true), PV::Int(n) | PV::Long(n)) => json!(*n as f64),
        (Some(true), PV::Float(f) | PV::Double(f)) => json!(f),
        (Some(true), PV::Bool(b)) => json!(if *b { 1.0 } else { 0.0 }),
        (Some(true), other) => {
            other.as_f64().map(|f| json!(f)).unwrap_or_else(|| json!(other.as_text()))
        }
        (_, PV::Str(s)) => json!(s.as_ref()),
        (_, PV::Date { .. }) => json!(v.as_text()),
        (_, other) => json!(other.as_text()),
    })
}
