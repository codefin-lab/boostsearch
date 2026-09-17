//! Reading a result set a page at a time, and the point in time a page is read from.

use super::*;

/// `_search/point_in_time` -- freeze what the indices hold now, so that
/// paging through them is not disturbed by writes that arrive meanwhile.
pub async fn create_pit(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let keep = p.get("keep_alive").map(|v| keep_alive_millis(v)).unwrap_or(0);
    match crate::cluster::search::open_pit(&store, &expr, keep, false) {
        Ok(opened) => respond(
            &p,
            json!({
                "pit_id": opened.id,
                "_shards": {
                    "total": opened.shards, "successful": opened.shards,
                    "skipped": 0, "failed": 0,
                },
                "creation_time": opened.created_ms,
            }),
        ),
        Err(r) => r,
    }
}

pub async fn get_all_pits(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    // the newest first, so the one a caller has just opened is the one it
    // reads about first
    let mut pits = crate::cluster::search::list_pits(&store);
    pits.sort_by_key(|p| std::cmp::Reverse(p.get("creation_time").and_then(|t| t.as_u64())));
    respond(&p, json!({"pits": pits}))
}

pub async fn delete_pit(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let ids: Vec<String> = match body.get("pit_id") {
        Some(Value::Array(a)) => {
            a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()
        }
        Some(Value::String(one)) => vec![one.clone()],
        _ => Vec::new(),
    };
    if ids.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: no pit ids specified;",
        );
    }
    let mut decoded = Vec::with_capacity(ids.len());
    for id in &ids {
        match crate::store::PitId::decode(id) {
            Some(d) => decoded.push((id.clone(), d)),
            None => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!("invalid id: [{id}]"),
                );
            }
        }
    }
    let pits: Vec<Value> = decoded
        .into_iter()
        .map(|(id, d)| {
            let gone = crate::cluster::search::close_pit(&store, &d);
            json!({"pit_id": id, "successful": gone})
        })
        .collect();
    respond(&p, json!({"pits": pits}))
}

/// `DELETE _search/point_in_time/_all` -- every point in time the caller may
/// let go of, on every node.
pub async fn delete_all_pits(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    let pits: Vec<Value> = crate::cluster::search::close_all_pits(&store)
        .into_iter()
        .map(|id| json!({"pit_id": id, "successful": true}))
        .collect();
    respond(&p, json!({"pits": pits}))
}

/// The answer for a method a point-in-time path does not take, as the
/// reference gives it.
fn wrong_method(method: &axum::http::Method, uri: &axum::http::Uri, allowed: &str) -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        axum::Json(json!({
            "error": format!(
                "Incorrect HTTP method for uri [{uri}] and method [{method}], allowed: [{allowed}]"
            ),
            "status": 405,
        })),
    )
        .into_response()
}

pub async fn pit_create_only(method: axum::http::Method, uri: axum::http::Uri) -> Response {
    wrong_method(&method, &uri, "POST")
}

pub async fn pit_delete_only(method: axum::http::Method, uri: axum::http::Uri) -> Response {
    wrong_method(&method, &uri, "DELETE")
}

pub async fn pit_list_or_delete(method: axum::http::Method, uri: axum::http::Uri) -> Response {
    wrong_method(&method, &uri, "GET, DELETE")
}

/// A request for the node that holds a search context, sent there as the
/// caller and answered as that node answers it.
pub(crate) async fn ask_holder(
    node: &str,
    method: axum::http::Method,
    uri: &str,
    body: &Value,
) -> Option<Response> {
    let rt = crate::cluster::runtime()?;
    let req = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .ok()?;
    let to = crate::cluster::NodeId(node.to_string());
    Some(crate::cluster::forward::forward(&rt, &to, req).await)
}

/// A node other than this one that a scroll id says holds the scroll.
fn held_elsewhere(id: &str) -> Option<String> {
    let rt = crate::cluster::runtime()?;
    let owner = Store::scroll_owner(id)?;
    (owner != rt.local().as_str()).then_some(owner)
}

/// The body of an answer, read as JSON.
async fn json_of(r: Response) -> (StatusCode, Value) {
    let status = r.status();
    let bytes = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap_or_default();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

pub(crate) fn check_scroll(
    store: &Store,
    expr: &str,
    body: &Value,
    p: &Params,
) -> Option<Response> {
    let keep = p.get("scroll")?;
    if body.get("size").and_then(|v| v.as_i64()) == Some(0)
        || p.get("size").map(|v| v == "0").unwrap_or(false)
    {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "[size] cannot be [0] in a scroll context",
        ));
    }
    if p.get("request_cache").map(|v| v == "true").unwrap_or(false) {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "[request_cache] cannot be used in a scroll context",
        ));
    }
    // a slice divides the documents between readers, and there is a ceiling on
    // how finely it may be cut
    if let Some(max) = body.pointer("/slice/max").and_then(|v| v.as_i64()) {
        // how finely a scroll may be cut is an index setting, so an index that
        // raises it may be sliced that far
        let limit = store
            .resolve(expr)
            .iter()
            .filter_map(|n| store.get(n))
            .filter_map(|st| st.read().numeric_setting("max_slices_per_scroll"))
            .max()
            .unwrap_or(1024) as i64;
        if max > limit {
            return Some(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!(
                    "The number of slices [{max}] is too large. It must be less than [{limit}]."
                ),
            ));
        }
    }
    // every scroll holds a point in time, and a point in time holds segments
    // open: there is a ceiling on how many may be open at once
    let open_limit = store
        .cluster_setting("search.max_open_scroll_context")
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .unwrap_or(crate::store::MAX_OPEN_SCROLLS as u64) as usize;
    store.sweep_contexts();
    if store.open_scrolls() >= open_limit {
        return Some(err(
            StatusCode::TOO_MANY_REQUESTS,
            "search_phase_execution_exception",
            format!(
                "Trying to create too many scroll contexts. Must be less than or equal to: \
                 [{open_limit}]. This limit can be set by changing the \
                 [search.max_open_scroll_context] setting."
            ),
        ));
    }
    let limit = store
        .cluster_setting("search.max_keep_alive")
        .and_then(|v| v.as_str().and_then(parse_keep_alive));
    if let (Some(limit), Some(want)) = (limit, parse_keep_alive(keep))
        && want > limit
    {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "Keep alive for request ({keep}) is too large. It must be less than ({}). \
                     This limit can be set by changing the [search.max_keep_alive] cluster level \
                     setting.",
                store
                    .cluster_setting("search.max_keep_alive")
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_default()
            ),
        ));
    }
    None
}

pub(crate) fn scroll_size(body: &Value, p: &Params) -> usize {
    body.get("size")
        .and_then(|v| v.as_u64())
        .or_else(|| p.get("size").and_then(|v| v.parse().ok()))
        .unwrap_or(10) as usize
}

pub async fn scroll(
    State(store): State<Store>,
    id_path: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let id = body
        .get("scroll_id")
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .or_else(|| p.get("scroll_id").cloned())
        .or_else(|| id_path.map(|Path(i)| i));
    let Some(id) = id else {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: scroll_id is missing;",
        );
    };
    // the ceiling applies every time the scroll is asked to live longer, not
    // only when it was opened
    let keep = body
        .get("scroll")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| p.get("scroll").cloned());
    if let Some(answer) = remote_scroll(&store, &id, keep.as_deref(), &p) {
        return answer;
    }
    // a scroll lives on the node that opened it, and the next batch may be
    // asked of any node: it is answered by the one that has it
    if let Some(owner) = held_elsewhere(&id) {
        let mut forwarded = json!({"scroll_id": id});
        if let Some(k) = &keep {
            forwarded["scroll"] = json!(k);
        }
        let query = if p.get("rest_total_hits_as_int").map(|v| v == "true").unwrap_or(false) {
            "?rest_total_hits_as_int=true"
        } else {
            ""
        };
        let uri = format!("/_search/scroll{query}");
        if let Some(r) = ask_holder(&owner, axum::http::Method::POST, &uri, &forwarded).await {
            // a holder that is gone took the scroll with it
            if r.status() == StatusCode::SERVICE_UNAVAILABLE {
                return err(
                    StatusCode::NOT_FOUND,
                    "search_context_missing_exception",
                    format!("No search context found for id [{id}]"),
                );
            }
            return r;
        }
    }
    let asked = body
        .get("scroll")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| p.get("scroll").cloned());
    if let (Some(keep), Some(limit)) = (
        asked.as_deref(),
        store
            .cluster_setting("search.max_keep_alive")
            .and_then(|v| v.as_str().map(|s| s.to_string())),
    ) && let (Some(want), Some(cap)) = (parse_keep_alive(keep), parse_keep_alive(&limit))
        && want > cap
    {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "Keep alive for request ({keep}) is too large. It must be less than \
                         ({limit}). This limit can be set by changing the \
                         [search.max_keep_alive] cluster level setting."
            ),
        );
    }
    let Some(state) = store.read_scroll(&id) else {
        return err(
            StatusCode::NOT_FOUND,
            "search_context_missing_exception",
            format!("No search context found for id [{id}]"),
        );
    };
    let mut req = state.body.clone();
    req["size"] = json!(state.size);
    // carried on from where the last batch ended; only a scroll opened before
    // there was a cursor counts from the beginning
    match &state.after {
        Some(after) => {
            req["search_after"] = json!(after);
            req["from"] = json!(0);
        }
        None => req["from"] = json!(state.offset),
    }
    // the scroll walks the index as it stood when it was opened, so a
    // document written since is not walked into halfway through
    if state.pit.is_empty() {
        if state.implicit_sort {
            req["sort"] = json!([{"_seq": "asc"}]);
        }
    } else {
        let renew = asked.clone().unwrap_or_else(|| "5m".to_string());
        req["pit"] = json!({"id": state.pit, "keep_alive": renew});
    }
    // a scroll is how a caller reads past the result window, so the window is
    // not what limits the batch it is reading now; the batch size was checked
    // when the scroll was opened
    let mut p = p;
    p.insert("scroll".to_string(), "1m".to_string());
    match crate::search::run(&store, &state.expr, &req, &p) {
        Ok(out) => {
            let n = out.hits.len();
            let mut env = crate::search::envelope(out, &req, &p);
            // a scroll that began counting from the beginning carries on that
            // way: a cursor taken up halfway would be read against an order
            // the caller chose, not the one the scroll was opened in
            let after =
                state.after.is_some().then(|| crate::api::search_api::last_sort_of(&env)).flatten();
            let renew = asked
                .as_deref()
                .and_then(parse_keep_alive)
                .map(|s| s * 1000)
                .unwrap_or(crate::store::DEFAULT_KEEP_ALIVE_MS);
            store.advance_scroll(&id, n, after, renew);
            if state.implicit_sort {
                crate::api::search_api::strip_sort(&mut env);
            }
            env["_scroll_id"] = json!(id);
            respond(&p, env)
        }
        Err(r) => r,
    }
}

pub async fn clear_scroll(
    State(store): State<Store>,
    id_path: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let mut ids: Vec<String> = match body.get("scroll_id") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => {
            a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()
        }
        _ => Vec::new(),
    };
    if let Some(Path(i)) = id_path {
        ids.extend(i.split(',').map(|s| s.to_string()));
    }
    if ids.iter().any(|i| i == "_all") {
        let mut n = store.close_all_scrolls();
        // every node holds its own scrolls; a node answering another's
        // request lets go of its own and asks nobody else
        if !crate::cluster::forward::answering_forward() {
            let me = crate::cluster::runtime().map(|rt| rt.local());
            let others: Vec<String> = match &me {
                Some(me) => crate::cluster::with_state(|s| {
                    s.nodes.keys().filter(|k| *k != me).map(|k| k.as_str().to_string()).collect()
                }),
                None => Vec::new(),
            };
            let all = json!({"scroll_id": ["_all"]});
            for node in others {
                if let Some(r) =
                    ask_holder(&node, axum::http::Method::DELETE, "/_search/scroll", &all).await
                {
                    let (_, v) = json_of(r).await;
                    n += v.get("num_freed").and_then(|f| f.as_u64()).unwrap_or(0) as usize;
                }
            }
        }
        return respond(&p, json!({"succeeded": true, "num_freed": n}));
    }
    if ids.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: no scroll ids specified;",
        );
    }
    let mut freed = 0usize;
    let mut elsewhere: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for id in &ids {
        match held_elsewhere(id) {
            Some(owner) => elsewhere.entry(owner).or_default().push(id.clone()),
            None => {
                if store.close_scroll(id) || clear_remote_scroll(&store, id) {
                    freed += 1;
                }
            }
        }
    }
    for (owner, held) in elsewhere {
        let asked = json!({"scroll_id": held});
        if let Some(r) =
            ask_holder(&owner, axum::http::Method::DELETE, "/_search/scroll", &asked).await
        {
            let (_, v) = json_of(r).await;
            freed += v.get("num_freed").and_then(|f| f.as_u64()).unwrap_or(0) as usize;
        }
    }
    // a scroll that was not there is not an error to report: the answer is
    // the ordinary one, with nothing freed, under the status that says so
    let body = json!({"succeeded": freed > 0, "num_freed": freed});
    if freed == 0 {
        let mut r = respond(&p, body);
        *r.status_mut() = StatusCode::NOT_FOUND;
        return r;
    }
    respond(&p, body)
}
