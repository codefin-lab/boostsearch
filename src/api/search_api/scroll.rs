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
    let names = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
    if names.is_empty() && !expr.is_empty() {
        return no_such_index(&expr);
    }
    let keep = p.get("keep_alive").map(|v| keep_alive_millis(v)).unwrap_or(0);
    let id = store.open_pit(&expr, keep);
    respond(
        &p,
        json!({
            "pit_id": id,
            "_shards": shards_over(&store, &names),
            "creation_time": 0,
        }),
    )
}

pub async fn get_all_pits(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    // the newest first: the ids are handed out in order, so the one a caller
    // has just opened is the one it reads about first
    let mut open = store.all_pits();
    open.sort_by(|a, b| b.0.cmp(&a.0));
    let pits: Vec<Value> = open
        .into_iter()
        .map(|(id, st)| {
            json!({
                "pit_id": id, "creation_time": 0, "keep_alive": st.keep_alive_ms,
            })
        })
        .collect();
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
        // no id names them all
        _ => store.all_pits().into_iter().map(|(id, _)| id).collect(),
    };
    let pits: Vec<Value> = ids
        .into_iter()
        .map(|id| {
            let gone = store.close_pit(&id);
            json!({"pit_id": id, "successful": gone})
        })
        .collect();
    respond(&p, json!({"pits": pits}))
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
    req["from"] = json!(state.offset);
    req["size"] = json!(state.size);
    // the scroll walks the index as it stood when it was opened, so a
    // document written since is not walked into halfway through
    req["pit"] = json!({"id": state.pit});
    match crate::search::run(&store, &state.expr, &req, &p) {
        Ok(out) => {
            let n = out.hits.len();
            store.advance_scroll(&id, n);
            let mut env = crate::search::envelope(out, &req, &p);
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
        let n = store.close_all_scrolls();
        return respond(&p, json!({"succeeded": true, "num_freed": n}));
    }
    if ids.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: no scroll ids specified;",
        );
    }
    let freed = ids.iter().filter(|i| store.close_scroll(i)).count();
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
