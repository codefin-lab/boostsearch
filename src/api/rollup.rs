//! `_plugins/_rollup/jobs` -- rollup jobs and what they have done.

use super::*;
use crate::ism::jobs;
use crate::ism::rollup;

fn not_found() -> Response {
    err(StatusCode::NOT_FOUND, "status_exception", "Rollup not found")
}

fn shown(held: &jobs::Held) -> Value {
    let mut r = held.body["rollup"].clone();
    if let Some(o) = r.as_object_mut() {
        o.remove("user");
        o.remove("roles");
    }
    r
}

fn held_rollup(store: &Store, id: &str) -> Result<jobs::Held, Response> {
    let held =
        jobs::read(store, id).filter(|h| h.body.get("rollup").is_some()).ok_or_else(not_found)?;
    if !jobs::may_touch(store, held.body["rollup"].get("user")) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "status_exception",
            format!("Do not have permission for rollup [{id}]"),
        ));
    }
    Ok(held)
}

/// `PUT _plugins/_rollup/jobs/{id}`
pub async fn put_rollup(
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
    let mut r = match rollup::parse(&id, &body, now) {
        Ok(r) => r,
        Err(refused) => return super::transform::refusal(refused),
    };
    let if_seq = match (
        p.get("if_seq_no").and_then(|v| v.parse::<u64>().ok()),
        p.get("if_primary_term").and_then(|v| v.parse::<u64>().ok()),
    ) {
        (Some(seq), Some(term)) => Some((seq, term)),
        _ => None,
    };
    let existing = jobs::read(&store, &id).filter(|h| h.body.get("rollup").is_some());
    if let (Some(_), Some(held)) = (if_seq, &existing) {
        let before = &held.body["rollup"];
        let fixed = ["source_index", "target_index", "continuous", "dimensions", "metrics"];
        let changed: Vec<&str> = fixed.iter().copied().filter(|k| before[*k] != r[*k]).collect();
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
                format!("Do not have permission for rollup [{id}]"),
            );
        }
        r["metadata_id"] = before["metadata_id"].clone();
        if let Some(user) = before.get("user") {
            r["user"] = user.clone();
        }
    }
    if existing.is_none()
        && let Some(user) = jobs::current_user(&store)
    {
        r["user"] = user;
    }
    match jobs::write(&store, &id, json!({"rollup": r}), if_seq.is_none(), if_seq) {
        Ok(held) => {
            jobs::forget("rollup", &id);
            let status = if existing.is_some() { StatusCode::OK } else { StatusCode::CREATED };
            let mut resp = (
                status,
                axum::Json(json!({
                    "_id": id,
                    "_version": held.version,
                    "_seq_no": held.seq_no,
                    "_primary_term": held.primary_term,
                    "rollup": shown(&held),
                })),
            )
                .into_response();
            if let Ok(v) =
                axum::http::HeaderValue::from_str(&format!("/_plugins/_rollup/jobs/{id}"))
            {
                resp.headers_mut().insert(axum::http::header::LOCATION, v);
            }
            resp
        }
        Err(resp) => resp,
    }
}

/// `GET _plugins/_rollup/jobs/{id}`, and the list of them without one.
pub async fn get_rollup(
    State(store): State<Store>,
    id: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let Some(Path(id)) = id else {
        return super::transform::list(&store, &p, "rollup");
    };
    match held_rollup(&store, &id) {
        Ok(held) => respond(
            &p,
            json!({
                "_id": id,
                "_version": held.version,
                "_seq_no": held.seq_no,
                "_primary_term": held.primary_term,
                "rollup": shown(&held),
            }),
        ),
        Err(r) => r,
    }
}

/// `DELETE _plugins/_rollup/jobs/{id}`
pub async fn delete_rollup(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let gone =
        || err(StatusCode::NOT_FOUND, "status_exception", format!("Rollup {id} is not found"));
    let Some(held) = jobs::read(&store, &id).filter(|h| h.body.get("rollup").is_some()) else {
        return gone();
    };
    if !jobs::may_touch(&store, held.body["rollup"].get("user")) {
        return err(
            StatusCode::FORBIDDEN,
            "status_exception",
            format!("Do not have permission for rollup [{id}]"),
        );
    }
    jobs::forget("rollup", &id);
    jobs::forget_metadata(&store, "rollup", &id);
    match jobs::delete(&store, &id) {
        Some(answer) => {
            let mut out = serde_json::Map::new();
            for key in ["_index", "_id", "_version", "result"] {
                out.insert(key.into(), answer[key].clone());
            }
            out.insert("forced_refresh".into(), json!(true));
            for key in ["_shards", "_seq_no", "_primary_term"] {
                out.insert(key.into(), answer[key].clone());
            }
            respond(&p, Value::Object(out))
        }
        None => gone(),
    }
}

/// `POST _plugins/_rollup/jobs/{id}/_start`
pub async fn start_rollup(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let held = match held_rollup(&store, &id) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let now = crate::store::now_millis();
    if held.body["rollup"]["enabled"].as_bool() != Some(true) {
        let mut body = held.body.clone();
        body["rollup"]["enabled"] = json!(true);
        body["rollup"]["enabled_time"] = json!(now);
        body["rollup"]["last_updated_time"] = json!(now);
        if let Err(r) = jobs::write(&store, &id, body, false, None) {
            return r;
        }
    }
    if let Some(meta_id) = held.body["rollup"]["metadata_id"].as_str()
        && let Some(meta) = jobs::read(&store, meta_id)
    {
        let status = meta.body["rollup_metadata"]["status"].as_str().unwrap_or_default();
        // a job that finished runs again from the beginning when it is started
        if matches!(status, "failed" | "stopped" | "finished") {
            let mut m = meta.body.clone();
            m["rollup_metadata"]["status"] = json!("started");
            m["rollup_metadata"]["failure_reason"] = Value::Null;
            m["rollup_metadata"]["last_updated_time"] = json!(now);
            let _ = jobs::write(&store, meta_id, m, false, None);
        }
    }
    respond(&p, json!({"acknowledged": true}))
}

/// `POST _plugins/_rollup/jobs/{id}/_stop`
pub async fn stop_rollup(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let held = match held_rollup(&store, &id) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let now = crate::store::now_millis();
    let mut body = held.body.clone();
    body["rollup"]["enabled"] = json!(false);
    body["rollup"]["enabled_time"] = Value::Null;
    body["rollup"]["last_updated_time"] = json!(now);
    if let Err(r) = jobs::write(&store, &id, body, false, None) {
        return r;
    }
    if let Some(meta_id) = held.body["rollup"]["metadata_id"].as_str()
        && let Some(meta) = jobs::read(&store, meta_id)
    {
        let status = meta.body["rollup_metadata"]["status"].as_str().unwrap_or_default();
        let next = match status {
            "started" | "init" | "stopped" => Some("stopped"),
            // a job that was retrying a failure goes back to having failed
            "retry" => Some("failed"),
            _ => None,
        };
        if let Some(next) = next {
            let mut m = meta.body.clone();
            m["rollup_metadata"]["status"] = json!(next);
            m["rollup_metadata"]["last_updated_time"] = json!(now);
            let _ = jobs::write(&store, meta_id, m, false, None);
        }
    }
    respond(&p, json!({"acknowledged": true}))
}

/// `GET _plugins/_rollup/jobs/{ids}/_explain`
pub async fn explain_rollup(
    State(store): State<Store>,
    Path(ids): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let mut out = serde_json::Map::new();
    for id in ids.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let visible = jobs::read(&store, id)
            .map(|h| jobs::may_touch(&store, h.body["rollup"].get("user")))
            .unwrap_or(true);
        out.insert(id.to_string(), if visible { rollup::explain(&store, id) } else { Value::Null });
    }
    respond(&p, Value::Object(out))
}
