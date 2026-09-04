//! `multi_terms`: one bucket per combination of several fields' values.

use super::*;

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
    let (peeled_subs, plain_subs) = split_peelable(&sub_aggs, store, targets);
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
