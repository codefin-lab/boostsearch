//! `_plugins/_ism` -- policies, and the indices under them.

use super::*;

/// `PUT _plugins/_ism/policies/{id}`
pub async fn put_policy(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if body.get("policy").is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "Missing [policy] in the request body",
        );
    }
    let existed = crate::ism::read(&store, &crate::ism::policy_id(&id)).is_some();
    // `?if_seq_no=` is how a caller says "only if nobody has changed it since"
    if let (Some(want), Some(held)) = (
        p.get("if_seq_no").and_then(|v| v.parse::<i64>().ok()),
        crate::ism::read(&store, &crate::ism::policy_id(&id))
            .and_then(|b| b.get("_seq_no").and_then(|v| v.as_i64())),
    ) && want != held
    {
        return err(
            StatusCode::CONFLICT,
            "version_conflict_engine_exception",
            format!("[{id}] has been changed since sequence number {want}"),
        );
    }
    let mut record = body.clone();
    let now = crate::store::now_millis();
    if let Some(policy) = record.get_mut("policy").and_then(|v| v.as_object_mut()) {
        policy.insert("policy_id".into(), json!(id));
        policy.insert("last_updated_time".into(), json!(now));
        policy.entry("schema_version").or_insert(json!(1));
    }
    let seq = now;
    record["_seq_no"] = json!(seq);
    if let Err(e) = crate::ism::put(&store, &crate::ism::policy_id(&id), record.clone()) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "illegal_state_exception", e);
    }
    let status = if existed { StatusCode::OK } else { StatusCode::CREATED };
    (
        status,
        axum::Json(json!({
            "_id": id,
            "_version": 1,
            "_seq_no": seq,
            "_primary_term": 1,
            "policy": record.get("policy").cloned().unwrap_or(json!({})),
        })),
    )
        .into_response()
}

/// `GET _plugins/_ism/policies/{id}`, and the whole list without one.
pub async fn get_policy(
    State(store): State<Store>,
    id: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let Some(Path(id)) = id else {
        let all: Vec<Value> = crate::ism::all(&store, "policy")
            .into_iter()
            .map(|(id, body)| {
                json!({
                    "_id": id.trim_start_matches("policy:"),
                    "_seq_no": body.get("_seq_no").cloned().unwrap_or(json!(0)),
                    "_primary_term": 1,
                    "policy": body.get("policy").cloned().unwrap_or(json!({})),
                })
            })
            .collect();
        return respond(&p, json!({"policies": all, "total_policies": all.len()}));
    };
    match crate::ism::read(&store, &crate::ism::policy_id(&id)) {
        Some(body) => respond(
            &p,
            json!({
                "_id": id,
                "_version": 1,
                "_seq_no": body.get("_seq_no").cloned().unwrap_or(json!(0)),
                "_primary_term": 1,
                "policy": body.get("policy").cloned().unwrap_or(json!({})),
            }),
        ),
        None => err(
            StatusCode::NOT_FOUND,
            "status_exception",
            format!("Policy with id {id} does not exist"),
        ),
    }
}

/// `DELETE _plugins/_ism/policies/{id}`
pub async fn delete_policy(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if !crate::ism::remove(&store, &crate::ism::policy_id(&id)) {
        return err(
            StatusCode::NOT_FOUND,
            "status_exception",
            format!("Policy with id {id} does not exist"),
        );
    }
    respond(
        &p,
        json!({"_id": id, "result": "deleted", "_shards": {"total": 1, "successful": 1, "failed": 0}}),
    )
}

/// `POST _plugins/_ism/add/{index}` -- put indices under a policy.
pub async fn add_policy(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let Some(policy) = body.get("policy_id").and_then(|v| v.as_str()) else {
        return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", "Missing policy_id");
    };
    each_index(&store, index, &p, |store, name| {
        if crate::ism::managed(store, name).is_some() {
            return Err(format!(
                "This index already has a policy, use the update policy API to update index policies"
            ));
        }
        crate::ism::attach(store, name, policy)
    })
}

/// `POST _plugins/_ism/remove/{index}` -- take them out from under it.
pub async fn remove_policy(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    each_index(&store, index, &p, |store, name| {
        match crate::ism::remove(store, &crate::ism::managed_id(name)) {
            true => Ok(()),
            false => Err("This index does not have a policy to remove".to_string()),
        }
    })
}

/// `POST _plugins/_ism/change_policy/{index}` -- swap the policy an index is
/// under, keeping it where it is unless told where to start.
pub async fn change_policy(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let Some(policy) = body.get("policy_id").and_then(|v| v.as_str()) else {
        return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", "Missing policy_id");
    };
    let state = body.get("state").and_then(|v| v.as_str()).map(|s| s.to_string());
    each_index(&store, index, &p, |store, name| {
        if crate::ism::managed(store, name).is_none() {
            return Err("This index does not have a policy".to_string());
        }
        crate::ism::attach(store, name, policy)?;
        if let Some(state) = &state
            && let Some(mut held) = crate::ism::managed(store, name)
        {
            held["managed_index"]["state"] =
                json!({"name": state, "start_time": crate::store::now_millis()});
            let _ = crate::ism::put(store, &crate::ism::managed_id(name), held);
        }
        Ok(())
    })
}

/// `POST _plugins/_ism/retry/{index}` -- try again what failed.
pub async fn retry_policy(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let state = body.get("state").and_then(|v| v.as_str()).map(|s| s.to_string());
    each_index(&store, index, &p, |store, name| {
        let Some(mut held) = crate::ism::managed(store, name) else {
            return Err("This index does not have a policy".to_string());
        };
        let failed = held.pointer("/managed_index/action/failed").and_then(|v| v.as_bool());
        if failed != Some(true) && state.is_none() {
            return Err("This index is not in a failed state".to_string());
        }
        held["managed_index"]["retry_count"] = json!(0);
        if let Some(action) = held.pointer_mut("/managed_index/action")
            && action.is_object()
        {
            action["failed"] = json!(false);
        }
        if let Some(state) = &state {
            held["managed_index"]["state"] =
                json!({"name": state, "start_time": crate::store::now_millis()});
            held["managed_index"]["action"] = Value::Null;
        }
        held["managed_index"]["info"] = json!({"message": "Attempting to retry"});
        crate::ism::put(store, &crate::ism::managed_id(name), held)
    })
}

/// `GET _plugins/_ism/explain/{index}` -- what each index is doing, and why.
pub async fn explain(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_else(|| "*".into());
    let names = match expr.as_str() {
        "" | "*" | "_all" => store.names(),
        other => store.resolve(other),
    };
    let mut out = serde_json::Map::new();
    let mut total = 0;
    for name in names {
        if name.starts_with('.') {
            continue;
        }
        let one = crate::ism::engine::explain(&store, &name);
        // an index with no policy is named with a null, which is how a caller
        // learns it is not managed rather than that it does not exist
        if one.get("policy_id").and_then(|v| v.as_str()).is_some() {
            total += 1;
        }
        out.insert(name, one);
    }
    out.insert("total_managed_indices".into(), json!(total));
    respond(&p, Value::Object(out))
}

/// Do something to every index a pattern names, and report per index the way
/// the index-management API does: a list of failures and a list of successes.
fn each_index(
    store: &Store,
    index: Option<Path<String>>,
    p: &Params,
    mut what: impl FnMut(&Store, &str) -> Result<(), String>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    if expr.is_empty() {
        return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", "Missing indices");
    }
    let names = store.resolve(&expr);
    if names.is_empty() {
        return err(
            StatusCode::NOT_FOUND,
            "index_not_found_exception",
            format!("no such index [{expr}]"),
        );
    }
    let mut failures = Vec::new();
    let mut updated = 0;
    for name in &names {
        match what(store, name) {
            Ok(()) => updated += 1,
            Err(reason) => failures.push(json!({"index_name": name, "index_uuid": store
                .get(name)
                .map(|st| st.read().uuid.clone())
                .unwrap_or_default(), "reason": reason})),
        }
    }
    respond(
        p,
        json!({
            "updated_indices": updated,
            "failures": !failures.is_empty(),
            "failed_indices": failures,
        }),
    )
}
