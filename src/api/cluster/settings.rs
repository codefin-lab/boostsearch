//! Settings that belong to the cluster rather than to an index.

use super::*;

/// `_cluster/voting_config_exclusions` -- nodes kept out of the vote that
/// elects a cluster manager.
///
/// One node has no election to hold, but the exclusions are still recorded
/// and reported, since a caller draining a node watches this list to know the
/// exclusion took.
pub async fn post_voting_config_exclusions(
    State(store): State<Store>,
    Query(p): Query<Params>,
) -> Response {
    let ids = p.get("node_ids").filter(|v| !v.is_empty());
    let names = p.get("node_names").or_else(|| p.get("node_name")).filter(|v| !v.is_empty());
    let entries: Vec<Value> = match (ids, names) {
        (Some(ids), None) => {
            ids.split(',').map(|n| json!({"node_id": n.trim(), "node_name": "_absent_"})).collect()
        }
        (None, Some(names)) => names
            .split(',')
            .map(|n| json!({"node_id": "_absent_", "node_name": n.trim()}))
            .collect(),
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                "Please set node identifiers correctly. One and only one of [node_name], \
                 [node_names] and [node_ids] has to be set",
            );
        }
    };
    store.add_voting_exclusions(entries);
    (StatusCode::OK, axum::Json(json!({}))).into_response()
}

pub async fn delete_voting_config_exclusions(State(store): State<Store>) -> Response {
    store.clear_voting_exclusions();
    (StatusCode::OK, axum::Json(json!({}))).into_response()
}

/// Walk a settings body into dotted keys with text values, whichever way the
/// caller wrote it.
pub(crate) fn flatten_cluster_settings(
    node: &Value,
    prefix: &str,
    out: &mut serde_json::Map<String, Value>,
) {
    match node {
        Value::Object(o) => {
            for (k, v) in o {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten_cluster_settings(v, &key, out);
            }
        }
        Value::Null => {
            out.insert(prefix.to_string(), Value::Null);
        }
        Value::String(s) => {
            out.insert(prefix.to_string(), json!(s));
        }
        other => {
            out.insert(prefix.to_string(), json!(other.to_string()));
        }
    }
}

/// A setting whose value the cluster refuses rather than stores.
pub(crate) fn check_cluster_setting(key: &str, value: &Value) -> Option<Response> {
    // a null is a removal, not a value, and nothing about it can be wrong
    if value.is_null() {
        return None;
    }
    // a cancellation rate or ratio of zero would cancel nothing, which is not
    // a setting so much as a way of turning the feature off by halves
    if key.starts_with("search_backpressure.") && key.contains("cancellation_") {
        let n =
            value.as_f64().or_else(|| value.as_str().and_then(|s| s.parse().ok())).unwrap_or(1.0);
        if n <= 0.0 {
            return Some(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("{key} must be > 0"),
            ));
        }
    }
    if key == "search_backpressure.mode" {
        let v = value.as_str().unwrap_or("");
        if !matches!(v, "monitor_only" | "enforced" | "disabled") {
            return Some(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("Invalid SearchBackpressureMode: {v}"),
            ));
        }
    }
    None
}

pub async fn cluster_settings_get(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    let raw = store.cluster_settings();
    let flat = p.get("flat_settings").map(|v| v == "true").unwrap_or(false);
    let view = |scope: &str| match raw.get(scope) {
        Some(v) if !flat => nest_settings(v),
        Some(v) => v.clone(),
        None => json!({}),
    };
    // `include_defaults` asks for the settings nobody set, which here is what
    // the node was started with
    let mut defaults = json!({});
    if flag(&p, "include_defaults") {
        for (k, v) in node_attrs() {
            defaults[format!("node.attr.{k}")] = json!(v);
        }
        if !flat {
            defaults = nest_settings(&defaults);
        }
    }
    respond(
        &p,
        json!({
            "persistent": view("persistent"),
            "transient": view("transient"),
            "defaults": defaults,
        }),
    )
}

pub async fn cluster_settings_put(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let mut body: Value = parse_body(&body).unwrap_or(json!({}));
    // settings arrive dotted or nested and are held one way: dotted, with the
    // value as text, which is how they are reported back
    for scope in ["persistent", "transient"] {
        let Some(o) = body.get(scope).and_then(|v| v.as_object()).cloned() else { continue };
        let mut flat = serde_json::Map::new();
        flatten_cluster_settings(&Value::Object(o), "", &mut flat);
        for (k, v) in &flat {
            if let Some(r) = check_cluster_setting(k, v) {
                return r;
            }
        }
        body[scope] = Value::Object(flat);
    }
    store.merge_cluster_settings(&body);
    // the answer is shaped the way the request asked to see settings
    let flat = p.get("flat_settings").map(|v| v == "true").unwrap_or(false);
    let echo = |scope: &str| match body.get(scope) {
        Some(v) if flat => {
            // a key set to null was a removal, and is not in the result
            let mut o = v.as_object().cloned().unwrap_or_default();
            o.retain(|_, val| !val.is_null());
            Value::Object(o)
        }
        Some(v) => nest_settings(v),
        None => json!({}),
    };
    respond(
        &p,
        json!({
            "acknowledged": true,
            "persistent": echo("persistent"),
            "transient": echo("transient"),
        }),
    )
}
