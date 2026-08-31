//! Nested objects: a document's objects are documents of their own here.

use super::*;

/// A nested aggregation counts the objects at a path, not the documents that
/// hold them, and everything under it works on those objects: a filter picks
/// objects, a terms aggregation groups them, a metric reads their fields.
///
/// The objects are read back from the documents the query matched, which is
/// the only place they are kept whole here.
pub(crate) fn run_nested_over_objects(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    path: &str,
    sub_aggs: &Option<Value>,
) -> std::result::Result<Value, Response> {
    let probe = json!({
        "query": main_query.clone().unwrap_or_else(|| json!({"match_all": {}})),
        "size": 10_000,
    });
    let answer = run(store, &targets.join(","), &probe, &Params::new())?;
    let mut objects: Vec<(String, Value)> = Vec::new();
    for hit in &answer.hits {
        let id = hit.get("_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let Some(source) = hit.get("_source") else { continue };
        gather_objects(source, path, &id, &mut objects);
    }
    Ok(objects_agg(store, targets, &objects, path, sub_aggs))
}

/// Every object at a path inside one document, however deeply the lists nest.
pub(crate) fn gather_objects(source: &Value, path: &str, id: &str, out: &mut Vec<(String, Value)>) {
    let mut here: Vec<&Value> = vec![source];
    for step in path.split('.') {
        let mut next = Vec::new();
        for node in here {
            match node.get(step) {
                Some(Value::Array(a)) => next.extend(a.iter()),
                Some(other) => next.push(other),
                None => {}
            }
        }
        here = next;
    }
    for object in here {
        out.push((id.to_string(), object.clone()));
    }
}

/// Run the aggregations written under a nested one over its objects.
pub(crate) fn objects_agg(
    store: &Store,
    targets: &[String],
    objects: &[(String, Value)],
    path: &str,
    sub_aggs: &Option<Value>,
) -> Value {
    let mut out = json!({"doc_count": objects.len()});
    let Some(reqs) = sub_aggs.as_ref().and_then(|s| s.as_object()) else { return out };
    for (name, def) in reqs {
        let subs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
        let field_of = |spec: &Value| -> String {
            let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("");
            field.strip_prefix(&format!("{path}.")).unwrap_or(field).to_string()
        };
        let values = |spec: &Value| -> Vec<(String, Value)> {
            let leaf = field_of(spec);
            objects
                .iter()
                .filter_map(|(id, o)| {
                    o.pointer(&format!("/{}", leaf.replace('.', "/")))
                        .map(|v| (id.clone(), v.clone()))
                })
                .collect()
        };
        if let Some(spec) = def.get("filter") {
            let kept: Vec<(String, Value)> = objects
                .iter()
                .filter(|(_, o)| object_matches(spec, o, path))
                .cloned()
                .collect();
            out[name.clone()] = objects_agg(store, targets, &kept, path, &subs);
        } else if let Some(spec) = def.get("nested") {
            let deeper = spec.get("path").and_then(|p| p.as_str()).unwrap_or("");
            let leaf = deeper.strip_prefix(&format!("{path}.")).unwrap_or(deeper);
            let mut inner = Vec::new();
            for (id, o) in objects {
                gather_objects(o, leaf, id, &mut inner);
            }
            out[name.clone()] = objects_agg(store, targets, &inner, deeper, &subs);
        } else if def.get("reverse_nested").is_some() {
            // back out to the documents the objects came from
            let mut seen: Vec<String> = Vec::new();
            for (id, _) in objects {
                if !seen.contains(id) {
                    seen.push(id.clone());
                }
            }
            let mut answer = json!({"doc_count": seen.len()});
            // above the objects the documents are documents again, and what is
            // asked of them is asked the ordinary way
            if let Some(subs) = subs.as_ref() {
                let narrowed = json!({"bool": {"filter": [{"terms": {"_id": seen}}]}});
                if let Ok((_, Some(Value::Object(inner)))) =
                    count_with_sub_aggs(store, targets, &narrowed, &Some(subs.clone()), false)
                {
                    for (k, v) in inner {
                        answer[k] = v;
                    }
                }
            }
            out[name.clone()] = answer;
        } else if let Some(spec) = def.get("terms") {
            let size = spec.get("size").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
            let min = spec.get("min_doc_count").and_then(|v| v.as_u64()).unwrap_or(1);
            let mut groups: Vec<(Value, Vec<(String, Value)>)> = Vec::new();
            for ((id, object), (_, value)) in objects.iter().zip(values(spec).into_iter()) {
                match groups.iter_mut().find(|(k, _)| *k == value) {
                    Some((_, list)) => list.push((id.clone(), object.clone())),
                    None => groups.push((value, vec![(id.clone(), object.clone())])),
                }
            }
            groups.sort_by(|a, b| {
                b.1.len().cmp(&a.1.len()).then_with(|| key_order(&a.0, &b.0))
            });
            let buckets: Vec<Value> = groups
                .into_iter()
                .filter(|(_, list)| list.len() as u64 >= min)
                .take(size)
                .map(|(key, list)| {
                    let mut b = json!({"key": key, "doc_count": list.len()});
                    if let Some(Value::Object(inner)) = subs
                        .as_ref()
                        .map(|_| objects_agg(store, targets, &list, path, &subs))
                    {
                        for (k, v) in inner {
                            if k != "doc_count" {
                                b[k] = v;
                            }
                        }
                    }
                    b
                })
                .collect();
            out[name.clone()] = json!({
                "doc_count_error_upper_bound": 0,
                "sum_other_doc_count": 0,
                "buckets": buckets,
            });
        } else if let Some(spec) = def.get("composite") {
            let sources: Vec<(String, Value)> = spec
                .get("sources")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|e| {
                            let (n, body) = e.as_object()?.iter().next()?;
                            Some((n.clone(), body.clone()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let size = spec.get("size").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
            // a marker says which page was already seen
            let after: Option<Vec<Value>> = spec.get("after").and_then(|a| a.as_object()).map(
                |o| sources.iter().map(|(n, _)| o.get(n).cloned().unwrap_or(Value::Null)).collect(),
            );
            let mut groups: Vec<(Vec<Value>, usize)> = Vec::new();
            for (_, object) in objects {
                let mut key = Vec::new();
                let mut whole = true;
                for (_, body) in &sources {
                    let inner = body.get("terms").unwrap_or(body);
                    let leaf = field_of(inner);
                    match object.pointer(&format!("/{}", leaf.replace('.', "/"))) {
                        Some(v) => key.push(v.clone()),
                        None => {
                            whole = false;
                            break;
                        }
                    }
                }
                if !whole {
                    continue;
                }
                match groups.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, n)) => *n += 1,
                    None => groups.push((key, 1)),
                }
            }
            groups.sort_by(|a, b| keys_order(&a.0, &b.0));
            if let Some(after) = after {
                groups.retain(|(key, _)| keys_order(key, &after) == Ordering::Greater);
            }
            let buckets: Vec<Value> = groups
                .into_iter()
                .take(size)
                .map(|(key, n)| {
                    let named: serde_json::Map<String, Value> = sources
                        .iter()
                        .map(|(name, _)| name.clone())
                        .zip(key.into_iter())
                        .collect();
                    json!({"key": Value::Object(named), "doc_count": n})
                })
                .collect();
            let mut answer = json!({"buckets": buckets});
            if let Some(last) = answer["buckets"].as_array().and_then(|a| a.last()) {
                answer["after_key"] = last["key"].clone();
            }
            out[name.clone()] = answer;
        } else {
            // a metric reads the objects' own values
            let kind = def
                .as_object()
                .and_then(|o| o.keys().map(|k| k.to_string()).next())
                .unwrap_or_default();
            let spec = def.get(&kind).cloned().unwrap_or(json!({}));
            let numbers: Vec<f64> =
                values(&spec).iter().filter_map(|(_, v)| number_of(v)).collect();
            let value = match kind.as_str() {
                "max" => numbers.iter().cloned().reduce(f64::max),
                "min" => numbers.iter().cloned().reduce(f64::min),
                "sum" => Some(numbers.iter().sum()),
                "avg" => (!numbers.is_empty())
                    .then(|| numbers.iter().sum::<f64>() / numbers.len() as f64),
                "value_count" => Some(numbers.len() as f64),
                _ => None,
            };
            if let Some(v) = value {
                out[name.clone()] = json!({"value": v});
            }
        }
    }
    out
}

/// A bucket key as text, for the cases that only need a name for it.
pub(crate) fn key_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Two bucket keys in order: numbers as numbers, everything else as text.
pub(crate) fn key_order(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&y.as_f64().unwrap_or(0.0))
            .unwrap_or(Ordering::Equal),
        _ => key_text(a).cmp(&key_text(b)),
    }
}

/// A list of keys in order, compared one after another.
pub(crate) fn keys_order(a: &[Value], b: &[Value]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let ord = key_order(x, y);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

/// Say which nested object a sort reads inside, wherever one is written under
/// an aggregation that has already entered that object.
pub(crate) fn scope_sorts_to(node: &mut Value, path: &str) {
    match node {
        Value::Object(o) => {
            if let Some(sort) = o.get_mut("sort") {
                let mut items = match sort.take() {
                    Value::Array(a) => a,
                    other => vec![other],
                };
                for item in items.iter_mut() {
                    match item {
                        Value::String(field) => {
                            let field = field.clone();
                            *item = json!({field: {"order": "asc", "nested": {"path": path}}});
                        }
                        Value::Object(keys) => {
                            for (_, opts) in keys.iter_mut() {
                                match opts {
                                    Value::String(order) => {
                                        let order = order.clone();
                                        *opts =
                                            json!({"order": order, "nested": {"path": path}});
                                    }
                                    Value::Object(oo) => {
                                        oo.insert("nested".into(), json!({"path": path}));
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
                *sort = Value::Array(items);
            }
            for (_, v) in o.iter_mut() {
                scope_sorts_to(v, path);
            }
        }
        Value::Array(a) => {
            for v in a {
                scope_sorts_to(v, path);
            }
        }
        _ => {}
    }
}

/// A value as the number it stands for: a date is its instant.
pub(crate) fn number_of(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        // read the text as written: folding it through the resolution the
        // index keeps would wrap a date far enough out
        Value::String(s) => boostcore::time::OffsetDateTime::parse(
            s,
            &boostcore::time::format_description::well_known::Rfc3339,
        )
        .ok()
        .map(|d| d.unix_timestamp_nanos() as f64)
        .or_else(|| {
            crate::store::parse_date_lenient(s).map(|d| d.unix_timestamp_nanos() as f64)
        })
        .or_else(|| s.parse().ok()),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Does one nested object match the filter a sort put on them?
///
/// Only the shapes a sort filter is written in are answered here: the boolean
/// wrappers, and the clauses that compare one of the object's own fields.
pub(crate) fn object_matches(filter: &Value, object: &Value, path: &str) -> bool {
    let Some((kind, body)) = filter.as_object().and_then(|o| o.iter().next()) else {
        return true;
    };
    let field_value = |name: &str| -> Option<Value> {
        let leaf = name.strip_prefix(&format!("{path}.")).unwrap_or(name);
        object.pointer(&format!("/{}", leaf.replace('.', "/"))).cloned()
    };
    match kind.as_str() {
        "bool" => {
            let all = |key: &str| -> bool {
                match body.get(key) {
                    None => true,
                    Some(Value::Array(a)) => a.iter().all(|c| object_matches(c, object, path)),
                    Some(one) => object_matches(one, object, path),
                }
            };
            let none = match body.get("must_not") {
                None => true,
                Some(Value::Array(a)) => !a.iter().any(|c| object_matches(c, object, path)),
                Some(one) => !object_matches(one, object, path),
            };
            all("filter") && all("must") && none
        }
        "match_all" => true,
        "exists" => body
            .get("field")
            .and_then(|f| f.as_str())
            .map(|f| field_value(f).is_some())
            .unwrap_or(false),
        "term" => {
            let Some((name, want)) = body.as_object().and_then(|o| o.iter().next()) else {
                return false;
            };
            let want = want.get("value").unwrap_or(want);
            field_value(name).map(|v| &v == want).unwrap_or(false)
        }
        // a match asks after the words in a value rather than the whole of it
        "match" | "match_phrase" => {
            let Some((name, want)) = body.as_object().and_then(|o| o.iter().next()) else {
                return false;
            };
            let want = want.get("query").unwrap_or(want);
            let Some(text) = field_value(name) else { return false };
            let text = match text {
                Value::String(s) => s.to_lowercase(),
                other => other.to_string().to_lowercase(),
            };
            let wanted = match want {
                Value::String(s) => s.to_lowercase(),
                other => other.to_string().to_lowercase(),
            };
            let words: Vec<&str> = text.split(|c: char| !c.is_alphanumeric()).collect();
            wanted
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
                .all(|w| words.contains(&w))
        }
        "range" => {
            let Some((name, spec)) = body.as_object().and_then(|o| o.iter().next()) else {
                return false;
            };
            let Some(here) = field_value(name).as_ref().and_then(number_of) else {
                return false;
            };
            let bound = |key: &str| -> Option<f64> {
                spec.get(key).and_then(|v| match v {
                    Value::String(s) => crate::store::canonical_date(&json!(s))
                        .and_then(|d| crate::store::parse_date_lenient(&d))
                        .map(|d| d.unix_timestamp_nanos() as f64)
                        .or_else(|| s.parse().ok()),
                    other => other.as_f64(),
                })
            };
            bound("gte").map(|b| here >= b).unwrap_or(true)
                && bound("gt").map(|b| here > b).unwrap_or(true)
                && bound("lte").map(|b| here <= b).unwrap_or(true)
                && bound("lt").map(|b| here < b).unwrap_or(true)
        }
        _ => true,
    }
}

/// Does this field live inside a nested object?
pub(crate) fn under_nested(mapping: &crate::store::Mapping, field: &str) -> bool {
    let mut walked = String::new();
    for part in field.split('.') {
        walked = if walked.is_empty() { part.to_string() } else { format!("{walked}.{part}") };
        if walked != field && mapping.type_of(&walked) == Some("nested") {
            return true;
        }
    }
    false
}

/// Every `nested` clause that asked for the objects it matched to be listed.
pub(crate) fn collect_nested_inner_hits(node: &Value, out: &mut Vec<(String, Value, Value)>) {
    match node {
        Value::Object(o) => {
            if let Some(nested) = o.get("nested") {
                if let Some(inner) = nested.get("inner_hits") {
                    let path =
                        nested.get("path").and_then(|p| p.as_str()).unwrap_or("").to_string();
                    let query = nested.get("query").cloned().unwrap_or(json!({}));
                    out.push((path, inner.clone(), query));
                }
            }
            for (_, v) in o {
                collect_nested_inner_hits(v, out);
            }
        }
        Value::Array(a) => {
            for v in a {
                collect_nested_inner_hits(v, out);
            }
        }
        _ => {}
    }
}

pub(crate) fn nested_inner_hits(
    h: &Hit,
    source: &Value,
    under: &str,
    clauses: &[(String, Value, Value)],
    kept: bool,
    query: &Option<Value>,
    mapping: &crate::store::Mapping,
    index: &boostcore::Index,
) -> serde_json::Map<String, Value> {
    let mut groups = serde_json::Map::new();
    for (path, inner, inner_query) in clauses {
        // a clause belongs here when what is left of its path after the object
        // it sits in names a field of that object
        let leaf = if under.is_empty() {
            path.clone()
        } else {
            match path.strip_prefix(&format!("{under}.")) {
                Some(rest) => rest.to_string(),
                None => continue,
            }
        };
        if leaf.is_empty() {
            continue;
        }
        let at = source.pointer(&format!("/{}", leaf.replace('.', "/")));
        let objects: Vec<Value> = match at {
            Some(Value::Array(a)) => a.clone(),
            Some(other) => vec![other.clone()],
            None => Vec::new(),
        };
        let size = inner.get("size").and_then(|v| v.as_u64()).unwrap_or(3) as usize;
        let objects: Vec<(usize, Value)> = objects
            .into_iter()
            .enumerate()
            .filter(|(_, o)| object_matches(inner_query, o, path))
            .collect();
        let mut list = Vec::new();
        for (offset, object) in objects.iter().cloned().take(size) {
            let mut one = json!({
                "_index": h.index.clone(),
                "_id": h.id.clone(),
                "_nested": {"field": path.clone(), "offset": offset},
                "_score": h.score,
            });
            if kept {
                one["_source"] = object.clone();
            }
            if inner.get("version").and_then(|v| v.as_bool()).unwrap_or(false) {
                one["_version"] = json!(h.version);
            }
            let asked: Vec<String> = match inner.get("docvalue_fields") {
                Some(Value::Array(a)) => a
                    .iter()
                    .filter_map(|v| {
                        v.as_str().map(|s| s.to_string()).or_else(|| {
                            v.get("field").and_then(|f| f.as_str()).map(|s| s.to_string())
                        })
                    })
                    .collect(),
                _ => Vec::new(),
            };
            if !asked.is_empty() {
                let mut fields = serde_json::Map::new();
                for name in asked {
                    if name == "_seq_no" {
                        fields.insert(name.clone(), json!([h.seq]));
                    } else if let Some(v) =
                        object.pointer(&format!("/{}", name.replace('.', "/")))
                    {
                        fields.insert(
                            name.clone(),
                            match v {
                                Value::Array(a) => Value::Array(a.clone()),
                                other => json!([other.clone()]),
                            },
                        );
                    }
                }
                one["fields"] = Value::Object(fields);
            }
            if let Some(spec) = inner.get("highlight") {
                // an inner hit is highlighted from its own object, under the
                // name the field is known by
                let mut here = json!({});
                let mut node = &mut here;
                let steps: Vec<&str> = path.split('.').collect();
                for step in &steps[..steps.len() - 1] {
                    node[*step] = json!({});
                    node = &mut node[*step];
                }
                node[steps[steps.len() - 1]] = object.clone();
                if let Some(hl) = build_highlight(spec, &here, query, mapping, index) {
                    one["highlight"] = hl;
                }
            }
            // whatever was asked of the objects under this one
            let deeper =
                nested_inner_hits(h, &object, path, clauses, kept, query, mapping, index);
            if !deeper.is_empty() {
                one["inner_hits"] = Value::Object(deeper);
            }
            list.push(one);
        }
        let name = inner.get("name").and_then(|n| n.as_str()).unwrap_or(path).to_string();
        groups.insert(
            name,
            json!({"hits": {
                "total": {"value": objects.len(), "relation": "eq"},
                "max_score": h.score,
                "hits": list,
            }}),
        );
    }
    groups
}

/// A `nested` clause that asked for the objects it matched to be listed: the
/// path, the inner-hits clause, and the query the objects have to match.
pub(crate) fn find_nested_inner_hits(node: &Value) -> Option<(String, Value)> {
    find_nested_inner_hits_full(node).map(|(p, i, _)| (p, i))
}

pub(crate) fn find_nested_inner_hits_full(node: &Value) -> Option<(String, Value, Value)> {
    match node {
        Value::Object(o) => {
            if let Some(nested) = o.get("nested") {
                if let Some(inner) = nested.get("inner_hits") {
                    let path =
                        nested.get("path").and_then(|p| p.as_str()).unwrap_or("").to_string();
                    let inner_query = nested.get("query").cloned().unwrap_or(json!({}));
                    return Some((path, inner.clone(), inner_query));
                }
            }
            o.values().find_map(find_nested_inner_hits_full)
        }
        Value::Array(a) => a.iter().find_map(find_nested_inner_hits_full),
        _ => None,
    }
}

/// Turn each hit into the nested objects it carries at a path.
pub(crate) fn expand_nested_hits(node: &mut Value, path: &str) {
    match node {
        Value::Object(o) => {
            if let Some(hits) = o.get_mut("hits").and_then(|h| h.get_mut("hits")) {
                if let Some(list) = hits.as_array().cloned() {
                    let mut out = Vec::new();
                    for hit in list {
                        let at = hit.pointer(&format!("/_source/{}", path.replace('.', "/")));
                        let objects: Vec<Value> = match at {
                            Some(Value::Array(a)) => a.clone(),
                            Some(other) => vec![other.clone()],
                            // with no source to read, the objects cannot be
                            // listed, but the hit still stands for one of them
                            None => {
                                let mut one = hit.clone();
                                one["_nested"] = json!({"field": path, "offset": 0});
                                out.push(one);
                                continue;
                            }
                        };
                        for (offset, object) in objects.into_iter().enumerate() {
                            let mut one = hit.clone();
                            one["_nested"] = json!({"field": path, "offset": offset});
                            if one.get("_source").is_some() {
                                one["_source"] = object;
                            }
                            out.push(one);
                        }
                    }
                    *hits = Value::Array(out);
                }
                return;
            }
            for (_, v) in o.iter_mut() {
                expand_nested_hits(v, path);
            }
        }
        Value::Array(a) => {
            for v in a {
                expand_nested_hits(v, path);
            }
        }
        _ => {}
    }
}
