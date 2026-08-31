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

pub(crate) fn data_stream_entry(_store: &Store, name: &str, template: &str) -> Value {
    json!({
        "name": name,
        "timestamp_field": {"name": "@timestamp"},
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
    if gone.is_empty() {
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
