//! The objects a nested query matched, returned beside the document.

use super::*;

/// Every `nested` clause that asked for the objects it matched to be listed.
pub(crate) fn collect_nested_inner_hits(node: &Value, out: &mut Vec<(String, Value, Value)>) {
    match node {
        Value::Object(o) => {
            if let Some(nested) = o.get("nested")
                && let Some(inner) = nested.get("inner_hits")
            {
                let path = nested.get("path").and_then(|p| p.as_str()).unwrap_or("").to_string();
                let query = nested.get("query").cloned().unwrap_or(json!({}));
                out.push((path, inner.clone(), query));
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

/// The inner-hits groups that belong directly under one object: those whose
/// path is the object's own path plus one step, each carrying the groups that
/// belong under *its* objects in turn.
#[allow(clippy::too_many_arguments)]
pub(crate) fn nested_inner_hits(
    h: &Hit,
    source: &Value,
    under: &str,
    clauses: &[(String, Value, Value)],
    kept: bool,
    query: &Option<Value>,
    mapping: &crate::store::Mapping,
    index: &boostcore::Index,
    analysis: &crate::analysis::Registry,
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
        for (offset, object) in objects.iter().take(size) {
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
                    } else if let Some(v) = object.pointer(&format!("/{}", name.replace('.', "/")))
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
                if let Some(hl) = build_highlight(spec, &here, query, mapping, index, analysis) {
                    one["highlight"] = hl;
                }
            }
            // whatever was asked of the objects under this one
            let deeper =
                nested_inner_hits(h, object, path, clauses, kept, query, mapping, index, analysis);
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
            if let Some(nested) = o.get("nested")
                && let Some(inner) = nested.get("inner_hits")
            {
                let path = nested.get("path").and_then(|p| p.as_str()).unwrap_or("").to_string();
                let inner_query = nested.get("query").cloned().unwrap_or(json!({}));
                return Some((path, inner.clone(), inner_query));
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
            // `hits.hits` is a page of documents only when it is a list: an
            // aggregation may itself be named `hits`, and then the `hits`
            // under it is that aggregation's own answer
            let page =
                o.get("hits").and_then(|h| h.get("hits")).map(|x| x.is_array()).unwrap_or(false);
            if page && let Some(hits) = o.get_mut("hits").and_then(|h| h.get_mut("hits")) {
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
