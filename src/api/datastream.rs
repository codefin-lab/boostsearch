//! Data streams: a name that writes to whichever index is current.

use super::*;

/// The composable template a data stream would be made from: the one whose
/// patterns the name fits, that says it backs a data stream, and that outranks
/// the others claiming the same name.
pub(crate) fn data_stream_template(store: &Store, name: &str) -> Option<(String, Value)> {
    let mut best: Option<(i64, String, Value)> = None;
    for (tname, t) in store.get_templates() {
        let Some(body) = t.get("__composable").cloned() else { continue };
        if body.get("data_stream").is_none() {
            continue;
        }
        let matches = body
            .get("index_patterns")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .any(|pat| pat == name || crate::store::glob_match(pat, name))
            })
            .unwrap_or(false);
        if !matches {
            continue;
        }
        let priority = body.get("priority").and_then(|v| v.as_i64()).unwrap_or(0);
        if best.as_ref().map(|(p, _, _)| priority > *p).unwrap_or(true) {
            best = Some((priority, tname, body));
        }
    }
    best.map(|(_, n, b)| (n, b))
}

/// The index a data stream's documents are actually written to.
pub(crate) fn backing_index(name: &str, generation: u64) -> String {
    format!(".ds-{name}-{generation:06}")
}

/// `PUT /_data_stream/{name}` -- a stream is an index that rolls over on its
/// own, so making one means making the index behind it.
pub async fn create_data_stream(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if store.data_streams().contains_key(&name) {
        return err(
            StatusCode::BAD_REQUEST,
            "resource_already_exists_exception",
            format!("data_stream [{name}] already exists"),
        );
    }
    let Some((template, _)) = data_stream_template(&store, &name) else {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("no matching index template found for data stream [{name}]"),
        );
    };
    if let Err(e) = store.create(&backing_index(&name, 1), &json!({})) {
        return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string());
    }
    store.add_data_stream(&name, &template);
    respond(&p, json!({"acknowledged": true}))
}

pub(crate) fn data_stream_entry(store: &Store, name: &str, template: &str) -> Value {
    // the template a stream was made from says which field carries its time
    let field = store
        .get_templates()
        .get(template)
        .and_then(|t| {
            // the composable form the template was written in is kept beside
            // the flattened one the index creation reads
            t.pointer("/__composable/data_stream/timestamp_field/name")
                .or_else(|| t.pointer("/__composable/data_stream/timestamp_field"))
                .or_else(|| t.pointer("/data_stream/timestamp_field/name"))
                .or_else(|| t.pointer("/data_stream/timestamp_field"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "@timestamp".to_string());
    json!({
        "name": name,
        "timestamp_field": {"name": field},
        "indices": [{
            "index_name": backing_index(name, 1),
            "index_uuid": crate::store::index_uuid(&backing_index(name, 1)),
        }],
        "generation": 1,
        "status": "GREEN",
        "template": template,
    })
}

/// `GET /_data_stream` and `GET /_data_stream/{name}`.
pub async fn get_data_stream(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let want = name.map(|Path(n)| n).unwrap_or_else(|| "*".into());
    let mut out: Vec<Value> = store
        .data_streams()
        .into_iter()
        .filter(|(n, _)| {
            want.split(',').any(|pat| {
                let pat = pat.trim();
                pat == "*" || pat == "_all" || pat == n || crate::store::glob_match(pat, n)
            })
        })
        .map(|(n, t)| data_stream_entry(&store, &n, &t))
        .collect();
    if out.is_empty() && !want.contains('*') && want != "_all" {
        return err(
            StatusCode::NOT_FOUND,
            "index_not_found_exception",
            format!("no such index [{want}]"),
        );
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    respond(&p, json!({"data_streams": out}))
}

/// `DELETE /_data_stream/{name}`.
pub async fn delete_data_stream(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let gone = store.remove_data_stream(&name);
    // a pattern that reaches no stream has taken none away
    if gone.is_empty() && !name.contains('*') && name != "_all" {
        return err(
            StatusCode::NOT_FOUND,
            "index_not_found_exception",
            format!("no such index [{name}]"),
        );
    }
    for g in &gone {
        store.delete(&backing_index(g, 1));
    }
    respond(&p, json!({"acknowledged": true}))
}

/// `GET /_data_stream/_stats` and `GET /_data_stream/{name}/_stats`.
///
/// What each stream holds: how many indices are behind it, how much they take
/// on disk, and the latest instant any of their documents carries.
pub async fn data_stream_stats(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let want = name.map(|Path(n)| n).unwrap_or_else(|| "*".into());
    let mut streams: Vec<Value> = Vec::new();
    let mut backing = 0usize;
    let mut total_bytes = 0u64;
    for (n, t) in store.data_streams() {
        let _ = &t;
        let named = want.split(',').any(|pat| {
            let pat = pat.trim();
            pat == "*" || pat == "_all" || pat == n || crate::store::glob_match(pat, &n)
        });
        if !named {
            continue;
        }
        let entry = data_stream_entry(&store, &n, &t);
        let indices: Vec<String> = entry["indices"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|i| i.get("index_name").and_then(|v| v.as_str()))
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        let mut bytes = 0u64;
        // the newest instant the stream's own time field carries
        let field = "@timestamp";
        let mut latest = 0i64;
        for idx in &indices {
            bytes += store.index_size(idx);
            let probe = json!({
                "size": 0,
                "aggs": {"newest": {"max": {"field": field}}},
            });
            if let Ok(found) = crate::search::run(&store, idx, &probe, &Params::new())
                && let Some(v) =
                    found.aggs.as_ref().and_then(|a| a.pointer("/newest/value")).and_then(|v| v.as_f64())
            {
                latest = latest.max(v as i64);
            }
        }
        backing += indices.len();
        total_bytes += bytes;
        streams.push(json!({
            "data_stream": n,
            "backing_indices": indices.len(),
            "store_size": crate::api::shared::readable_bytes(bytes),
            "store_size_bytes": bytes,
            "maximum_timestamp": latest,
        }));
    }
    streams.sort_by(|a, b| a["data_stream"].as_str().cmp(&b["data_stream"].as_str()));
    respond(
        &p,
        json!({
            "_shards": {"total": backing, "successful": backing, "failed": 0},
            "data_stream_count": streams.len(),
            "backing_indices": backing,
            "total_store_size": crate::api::shared::readable_bytes(total_bytes),
            "total_store_size_bytes": total_bytes,
            "data_streams": streams,
        }),
    )
}
