//! `_plugins/_sm` -- the policies that take snapshots and throw them away.

use super::*;
use crate::ism::{jobs, sm};

fn not_found() -> Response {
    err(StatusCode::NOT_FOUND, "status_exception", "Snapshot management policy not found")
}

/// A refusal of the model's, in the shape the plugin reports one.
fn refusal((status, kind, reason): crate::ism::transform::Refusal) -> Response {
    err(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST), &kind, reason)
}

/// `POST _plugins/_sm/policies/{name}` -- a policy that is not there yet.
pub async fn create_sm_policy(
    State(store): State<Store>,
    Path(name): Path<String>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let now = crate::store::now_millis();
    let policy = match sm::parse(&name, &body, now, None) {
        Ok(p) => p,
        Err(r) => return refusal(r),
    };
    let id = sm::policy_id(&name);
    // a create is a create: a name already taken is a conflict, reported the
    // way a write to a document that exists is reported
    match jobs::write(&store, &id, json!({"sm_policy": policy}), true, None) {
        Ok(held) => {
            sm::forget(&store, &name);
            (StatusCode::CREATED, axum::Json(shown(&id, &held))).into_response()
        }
        Err(r) => r,
    }
}

/// `PUT _plugins/_sm/policies/{name}` -- a policy that is already there, and
/// only over the version the caller has read.
pub async fn update_sm_policy(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    // An update is over what the caller has read, always: the plugin refuses
    // one that does not say which version it read, rather than writing over
    // whatever is there now.
    let if_seq = match (
        p.get("if_seq_no").and_then(|v| v.parse::<u64>().ok()),
        p.get("if_primary_term").and_then(|v| v.parse::<u64>().ok()),
    ) {
        (Some(seq), Some(term)) => (seq, term),
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "action_request_validation_exception",
                "Validation Failed: 1: Sequence number and primary term must be provided when \
                 updating a snapshot management policy;",
            );
        }
    };
    let Some(before) = sm::held(&store, &name) else { return not_found() };
    let now = crate::store::now_millis();
    let policy = match sm::parse(&name, &body, now, Some(&before.body["sm_policy"])) {
        Ok(p) => p,
        Err(r) => return refusal(r),
    };
    let id = sm::policy_id(&name);
    match jobs::write(&store, &id, json!({"sm_policy": policy}), false, Some(if_seq)) {
        Ok(held) => {
            sm::forget(&store, &name);
            respond(&p, shown(&id, &held))
        }
        Err(r) => r,
    }
}

/// `GET _plugins/_sm/policies/{name}`, and the list of them without one.
pub async fn get_sm_policy(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let Some(Path(name)) = name else { return list(&store, &p) };
    match sm::held(&store, &name) {
        Some(held) => respond(&p, shown(&sm::policy_id(&name), &held)),
        None => not_found(),
    }
}

/// `DELETE _plugins/_sm/policies/{name}`
pub async fn delete_sm_policy(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if sm::held(&store, &name).is_none() {
        return not_found();
    }
    let id = sm::policy_id(&name);
    let Some(mut answer) = jobs::delete(&store, &id) else { return not_found() };
    sm::forget(&store, &name);
    answer["forced_refresh"] = json!(true);
    respond(&p, delete_order(answer))
}

/// A delete's answer with its fields in the order the plugin writes them.
fn delete_order(answer: Value) -> Value {
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
    ] {
        if let Some(v) = answer.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    Value::Object(out)
}

/// `POST _plugins/_sm/policies/{name}/_start`
pub async fn start_sm_policy(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let Some(held) = sm::held(&store, &name) else { return not_found() };
    let now = crate::store::now_millis();
    let mut body = held.body.clone();
    if body["sm_policy"]["enabled"].as_bool() != Some(true) {
        body["sm_policy"]["enabled"] = json!(true);
        body["sm_policy"]["enabled_time"] = json!(now);
        body["sm_policy"]["last_updated_time"] = json!(now);
        if let Err(r) = jobs::write(&store, &sm::policy_id(&name), body, false, None) {
            return r;
        }
        // a policy started again is scheduled afresh from now, rather than
        // firing at once for every run it slept through
        sm::forget(&store, &name);
    }
    respond(&p, json!({"acknowledged": true}))
}

/// `POST _plugins/_sm/policies/{name}/_stop`
pub async fn stop_sm_policy(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let Some(held) = sm::held(&store, &name) else { return not_found() };
    let now = crate::store::now_millis();
    let mut body = held.body.clone();
    if body["sm_policy"]["enabled"].as_bool() != Some(false) {
        body["sm_policy"]["enabled"] = json!(false);
        body["sm_policy"]["enabled_time"] = Value::Null;
        body["sm_policy"]["last_updated_time"] = json!(now);
        if let Err(r) = jobs::write(&store, &sm::policy_id(&name), body, false, None) {
            return r;
        }
    }
    respond(&p, json!({"acknowledged": true}))
}

/// `GET _plugins/_sm/policies/{names}/_explain` -- what each policy is doing.
///
/// A name that reaches no policy is left out rather than reported: the answer
/// is the policies there are, and an empty list is how a caller learns there
/// are none.
pub async fn explain_sm_policy(
    State(store): State<Store>,
    Path(names): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let wanted: Vec<&str> = names.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    // `_explain` with no name at all is not a request for every policy: the
    // plugin has no such route, and answers that the policy was not found
    if wanted.is_empty() {
        return not_found();
    }
    let mut out: Vec<Value> = Vec::new();
    for (id, held) in sm::all(&store) {
        let name = sm::name_of(&id);
        let named =
            wanted.iter().any(|want| *want == name || crate::store::glob_match(want, &name));
        if named {
            out.push(sm::explain(&store, &name, &held));
        }
    }
    // a name that is not a pattern and reaches nothing is a policy that is
    // not there, which is what the plugin says of it
    if out.is_empty() && !wanted.iter().any(|want| want.contains('*')) {
        return not_found();
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    respond(&p, json!({"policies": out}))
}

/// The policies there are, a page of them in the order asked for.
fn list(store: &Store, p: &Params) -> Response {
    let from = p.get("from").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    let size = p.get("size").and_then(|v| v.parse::<usize>().ok()).unwrap_or(20);
    let search = p.get("queryString").map(|s| s.trim().to_string()).unwrap_or_default();
    let sort_field = p.get("sortField").cloned().unwrap_or_else(|| "sm_policy.name".to_string());
    let descending = p.get("sortOrder").map(|d| d.eq_ignore_ascii_case("desc")).unwrap_or(false);
    let pattern = if search.contains('*') { search.clone() } else { format!("*{search}*") };
    let mut found: Vec<(String, jobs::Held)> = sm::all(store)
        .into_iter()
        .filter(|(id, _)| search.is_empty() || crate::store::glob_match(&pattern, &sm::name_of(id)))
        .collect();
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
        .map(|(id, held)| {
            json!({
                "_id": id,
                "_seq_no": held.seq_no,
                "_primary_term": held.primary_term,
                "sm_policy": held.body["sm_policy"].clone(),
            })
        })
        .collect();
    respond(p, json!({"policies": page, "total_policies": total}))
}

/// A stored policy as the API answers it.
fn shown(id: &str, held: &jobs::Held) -> Value {
    json!({
        "_id": id,
        "_version": held.version,
        "_seq_no": held.seq_no,
        "_primary_term": held.primary_term,
        "sm_policy": held.body["sm_policy"].clone(),
    })
}
