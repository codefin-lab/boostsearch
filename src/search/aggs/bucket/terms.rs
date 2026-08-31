//! The aggregations that bucket by the values a field holds.

use super::*;

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
