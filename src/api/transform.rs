//! `_plugins/_transform` -- summary indices kept from a source index.

use super::*;
use crate::ism::jobs;
use crate::ism::transform;

/// A refusal of the model's, answered the way the plugin answers it.
pub(crate) fn refusal((status, kind, reason): transform::Refusal) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
    // a body with nothing in it is refused by the parser, which says where
    // it stopped: at the closing brace of an empty object
    if kind == "parsing_exception" && reason.starts_with("Failed to parse object") {
        let cause = json!({"type": kind, "reason": reason, "line": 1, "col": 2});
        let mut error = cause.clone();
        error["root_cause"] = json!([cause]);
        return (status, axum::Json(json!({"error": error, "status": status.as_u16()})))
            .into_response();
    }
    err(status, &kind, reason)
}

fn not_found() -> Response {
    err(StatusCode::NOT_FOUND, "status_exception", "Transform not found")
}

/// A stored transform as the API shows it: without the user who wrote it.
fn shown(held: &jobs::Held) -> Value {
    let mut t = held.body["transform"].clone();
    if let Some(o) = t.as_object_mut() {
        o.remove("user");
        o.remove("roles");
    }
    t
}

/// A transform the caller may see, or the answer to give instead.
fn held_transform(store: &Store, id: &str) -> Result<jobs::Held, Response> {
    let held = jobs::read(store, id)
        .filter(|h| h.body.get("transform").is_some())
        .ok_or_else(not_found)?;
    if !jobs::may_touch(store, held.body["transform"].get("user")) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "status_exception",
            format!("Do not have permission for transform [{id}]"),
        ));
    }
    Ok(held)
}

/// `PUT _plugins/_transform/{id}`
pub async fn put_transform(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if let Some(r) = jobs::user_configuration_refusal(&store) {
        return r;
    }
    let now = crate::store::now_millis();
    let mut t = match transform::parse(&id, &body, now) {
        Ok(t) => t,
        Err(r) => return refusal(r),
    };
    let if_seq = match (
        p.get("if_seq_no").and_then(|v| v.parse::<u64>().ok()),
        p.get("if_primary_term").and_then(|v| v.parse::<u64>().ok()),
    ) {
        (Some(seq), Some(term)) => Some((seq, term)),
        _ => None,
    };
    let existing = jobs::read(&store, &id).filter(|h| h.body.get("transform").is_some());
    if let (Some(_), Some(held)) = (if_seq, &existing) {
        // what a transform is -- where it reads, where it writes, what it
        // groups and computes -- is fixed once it exists; the rest may change
        let before = &held.body["transform"];
        let fixed = [
            "source_index",
            "target_index",
            "groups",
            "aggregations",
            "data_selection_query",
            "continuous",
        ];
        let changed: Vec<&str> = fixed.iter().copied().filter(|k| before[*k] != t[*k]).collect();
        if !changed.is_empty() {
            return err(
                StatusCode::BAD_REQUEST,
                "status_exception",
                format!("Not allowed to modify [{}]", changed.join(", ")),
            );
        }
        if !jobs::may_touch(&store, before.get("user")) {
            return err(
                StatusCode::FORBIDDEN,
                "status_exception",
                format!("Do not have permission for transform [{id}]"),
            );
        }
        t["metadata_id"] = before["metadata_id"].clone();
        if let Some(user) = before.get("user") {
            t["user"] = user.clone();
        }
        if before["enabled"] == t["enabled"] && t["enabled"] == json!(true) {
            t["enabled_at"] = before["enabled_at"].clone();
        }
    } else if let Err(r) = transform::validate(&store, &t) {
        return refusal(r);
    }
    if existing.is_none()
        && let Some(user) = jobs::current_user(&store)
    {
        t["user"] = user;
    }
    let create = if_seq.is_none();
    match jobs::write(&store, &id, json!({"transform": t}), create, if_seq) {
        Ok(held) => {
            jobs::forget("transform", &id);
            let status = if existing.is_some() { StatusCode::OK } else { StatusCode::CREATED };
            let mut r = (
                status,
                axum::Json(json!({
                    "_id": id,
                    "_version": held.version,
                    "_seq_no": held.seq_no,
                    "_primary_term": held.primary_term,
                    "transform": shown(&held),
                })),
            )
                .into_response();
            if let Ok(v) = axum::http::HeaderValue::from_str(&format!("/_plugins/_transform/{id}"))
            {
                r.headers_mut().insert(axum::http::header::LOCATION, v);
            }
            r
        }
        Err(r) => r,
    }
}

/// `GET _plugins/_transform/{id}`, and the list of them without one.
pub async fn get_transform(
    State(store): State<Store>,
    id: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let Some(Path(id)) = id else {
        return list(&store, &p, "transform");
    };
    match held_transform(&store, &id) {
        Ok(held) => respond(
            &p,
            json!({
                "_id": id,
                "_version": held.version,
                "_seq_no": held.seq_no,
                "_primary_term": held.primary_term,
                "transform": shown(&held),
            }),
        ),
        Err(r) => r,
    }
}

/// The list of transforms or rollups: those whose id matches the search, in
/// the order asked for, a page of them.
pub(crate) fn list(store: &Store, p: &Params, kind: &str) -> Response {
    let search = p.get("search").map(|s| s.trim().to_string()).unwrap_or_default();
    let from = p.get("from").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    let size = p.get("size").and_then(|v| v.parse::<usize>().ok()).unwrap_or(20);
    let id_field = format!("{kind}_id");
    let default_sort = format!("{kind}.{id_field}.keyword");
    let sort_field = p.get("sortField").cloned().unwrap_or(default_sort);
    let descending =
        p.get("sortDirection").map(|d| d.eq_ignore_ascii_case("desc")).unwrap_or(false);
    let pattern = format!("*{search}*");
    let mut found: Vec<(String, jobs::Held)> = jobs::all(store, kind)
        .into_iter()
        .filter(|(id, _)| search.is_empty() || crate::store::glob_match(&pattern, id))
        .filter(|(_, h)| jobs::may_touch(store, h.body[kind].get("user")))
        .collect();
    // the sort field is a path into the job document, with `.keyword` naming
    // the field's exact value
    let path = sort_field.trim_end_matches(".keyword").replace('.', "/");
    let key_of =
        |h: &jobs::Held| h.body.pointer(&format!("/{path}")).cloned().unwrap_or(Value::Null);
    found.sort_by(|a, b| {
        let (x, y) = (key_of(&a.1), key_of(&b.1));
        let order = match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
            _ => x.to_string().cmp(&y.to_string()),
        };
        if descending { order.reverse() } else { order }
    });
    let total = found.len();
    let page: Vec<Value> = found
        .into_iter()
        .skip(from)
        .take(size)
        .map(|(id, h)| {
            let mut job = h.body[kind].clone();
            if let Some(o) = job.as_object_mut() {
                o.remove("user");
                o.remove("roles");
            }
            json!({"_id": id, "_seq_no": h.seq_no, "_primary_term": h.primary_term, kind: job})
        })
        .collect();
    let (total_key, list_key) = match kind {
        "transform" => ("total_transforms", "transforms"),
        _ => ("total_rollups", "rollups"),
    };
    respond(p, json!({total_key: total, list_key: page}))
}

/// `DELETE _plugins/_transform/{ids}` -- refused for a transform still
/// enabled unless `force` says otherwise.
pub async fn delete_transform(
    State(store): State<Store>,
    Path(ids): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let started = std::time::Instant::now();
    let ids: Vec<String> =
        ids.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let force = p.get("force").map(|v| v != "false").unwrap_or(false);
    let mut enabled = Vec::new();
    for id in &ids {
        if let Some(held) = jobs::read(&store, id).filter(|h| h.body.get("transform").is_some()) {
            if !jobs::may_touch(&store, held.body["transform"].get("user")) {
                return err(
                    StatusCode::FORBIDDEN,
                    "status_exception",
                    format!(
                        "Don't have permission to delete some/all transforms in [{}]",
                        ids.join(", ")
                    ),
                );
            }
            if held.body["transform"]["enabled"].as_bool() == Some(true) {
                enabled.push(id.clone());
            }
        }
    }
    if !enabled.is_empty() && !force {
        return err(
            StatusCode::CONFLICT,
            "status_exception",
            format!(
                "[{}] transform(s) are enabled, please disable them before deleting them or set force flag",
                enabled.join(", ")
            ),
        );
    }
    let mut items = Vec::new();
    for id in &ids {
        jobs::forget("transform", id);
        jobs::forget_metadata(&store, "transform", id);
        let found = jobs::read(&store, id).filter(|h| h.body.get("transform").is_some());
        let item = match found.and_then(|_| jobs::delete(&store, id)) {
            Some(mut answer) => {
                answer["forced_refresh"] = json!(true);
                answer["status"] = json!(200);
                reorder_delete(answer)
            }
            None => json!({"_index": crate::ism::CONFIG_INDEX, "_id": id, "_version": 1,
                "result": "not_found", "forced_refresh": true,
                "_shards": {"total": 1, "successful": 1, "failed": 0},
                "_seq_no": 0, "_primary_term": 1, "status": 404}),
        };
        items.push(json!({"delete": item}));
    }
    respond(
        &p,
        json!({"took": started.elapsed().as_millis() as u64, "errors": false, "items": items}),
    )
}

/// A delete's answer with its fields in the order the reference writes them.
fn reorder_delete(answer: Value) -> Value {
    let mut out = serde_json::Map::new();
    for key in [
        "_index",
        "_id",
        "_version",
        "result",
        "forced_refresh",
        "_shards",
        "_seq_no",
        "_primary_term",
        "status",
    ] {
        if let Some(v) = answer.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    Value::Object(out)
}

/// `POST _plugins/_transform/{id}/_start`
pub async fn start_transform(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let held = match held_transform(&store, &id) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let t = &held.body["transform"];
    let now = crate::store::now_millis();
    if t["enabled"].as_bool() != Some(true) {
        let mut body = held.body.clone();
        body["transform"]["enabled"] = json!(true);
        body["transform"]["enabled_at"] = json!(now);
        body["transform"]["updated_at"] = json!(now);
        if let Err(r) = jobs::write(&store, &id, body, false, None) {
            return r;
        }
    }
    // a transform that failed, stopped or finished starts again from where
    // its metadata says it is, with the reason it failed cleared
    if let Some(meta) = jobs::metadata_of(&store, "transform", &id, t["metadata_id"].as_str()) {
        let m = &meta.body["transform_metadata"];
        if matches!(m["status"].as_str(), Some("failed" | "stopped" | "finished")) {
            let mut body = meta.body.clone();
            body["transform_metadata"]["status"] = json!("started");
            body["transform_metadata"]["failure_reason"] = Value::Null;
            body["transform_metadata"]["last_updated_at"] = json!(now);
            if let Some(meta_id) = t["metadata_id"].as_str() {
                let _ = jobs::write(&store, meta_id, body, false, None);
            }
        }
    }
    respond(&p, json!({"acknowledged": true}))
}

/// `POST _plugins/_transform/{id}/_stop`
pub async fn stop_transform(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let held = match held_transform(&store, &id) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let now = crate::store::now_millis();
    let mut body = held.body.clone();
    body["transform"]["enabled"] = json!(false);
    body["transform"]["enabled_at"] = Value::Null;
    body["transform"]["updated_at"] = json!(now);
    if let Err(r) = jobs::write(&store, &id, body, false, None) {
        return r;
    }
    if let Some(meta_id) = held.body["transform"]["metadata_id"].as_str()
        && let Some(meta) = jobs::read(&store, meta_id)
    {
        let status = meta.body["transform_metadata"]["status"].as_str().unwrap_or_default();
        if matches!(status, "started" | "init" | "stopped") {
            let mut m = meta.body.clone();
            m["transform_metadata"]["status"] = json!("stopped");
            m["transform_metadata"]["last_updated_at"] = json!(now);
            let _ = jobs::write(&store, meta_id, m, false, None);
        }
    }
    respond(&p, json!({"acknowledged": true}))
}

/// `GET _plugins/_transform/{ids}/_explain`
pub async fn explain_transform(
    State(store): State<Store>,
    Path(ids): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let mut out = serde_json::Map::new();
    for id in ids.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let visible = jobs::read(&store, id)
            .map(|h| jobs::may_touch(&store, h.body["transform"].get("user")))
            .unwrap_or(true);
        out.insert(
            id.to_string(),
            if visible { transform::explain(&store, id) } else { Value::Null },
        );
    }
    respond(&p, Value::Object(out))
}

/// `POST _plugins/_transform/_preview`
pub async fn preview_transform(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let id = body
        .pointer("/transform/transform_id")
        .and_then(|v| v.as_str())
        .unwrap_or("preview")
        .to_string();
    let t = match transform::parse(&id, &body, crate::store::now_millis()) {
        Ok(t) => t,
        Err(r) => return refusal(r),
    };
    if let Err(r) = transform::validate(&store, &t) {
        return refusal(r);
    }
    let user = jobs::current_user(&store);
    let store2 = store.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        jobs::run_as(user.as_ref(), || transform::preview(&store2, &t))
    })
    .await;
    match outcome {
        Ok(Ok(docs)) => respond(&p, json!({"documents": docs})),
        Ok(Err(reason)) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "status_exception",
            format!("Failed to parse the transformed results: {reason}"),
        ),
        Err(_) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "status_exception",
            "Failed to parse the transformed results",
        ),
    }
}
