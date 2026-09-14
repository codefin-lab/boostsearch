//! The endpoints that search: `_search` and everything that stands beside it.

use super::*;

mod analyze_api;
pub use analyze_api::*;
mod field_caps;
pub use field_caps::*;
mod scroll;
pub use scroll::*;

pub async fn search(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let raw = body.clone();
    let answer = search_answer(State(store), index, Query(p), body).await;
    with_position(answer, &raw).await
}

/// A refusal to parse a field, given the place in the body the reference
/// would name. See `json_position`.
async fn with_position(answer: Response, raw: &str) -> Response {
    if answer.status() != StatusCode::BAD_REQUEST {
        return answer;
    }
    let (parts, body) = answer.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 1024 * 1024).await else {
        return err(StatusCode::BAD_REQUEST, "parsing_exception", "failed to parse the request");
    };
    let rebuilt = |bytes: axum::body::Bytes| {
        let mut parts = parts.clone();
        parts.headers.remove(axum::http::header::CONTENT_LENGTH);
        Response::from_parts(parts, axum::body::Body::from(bytes))
    };
    let Ok(mut v) = serde_json::from_slice::<Value>(&bytes) else { return rebuilt(bytes) };
    let is_it =
        v.pointer("/error/type").and_then(|t| t.as_str()) == Some("x_content_parse_exception");
    let reason = v.pointer("/error/reason").and_then(|t| t.as_str()).unwrap_or("").to_string();
    // `[terms] failed to parse field [exclude]`, and not one already placed
    let named =
        reason.strip_prefix('[').and_then(|r| r.split_once("] failed to parse field [")).and_then(
            |(owner, rest)| rest.strip_suffix(']').map(|f| (owner.to_string(), f.to_string())),
        );
    let Some((owner, field)) = named.filter(|_| is_it) else { return rebuilt(bytes) };
    let Some((line, col)) = crate::api::json_position::locate(raw, &owner, &field) else {
        return rebuilt(bytes);
    };
    let placed = format!("[{line}:{col}] {reason}");
    v["error"]["reason"] = json!(placed);
    if let Some(roots) = v.pointer_mut("/error/root_cause").and_then(|r| r.as_array_mut()) {
        for root in roots {
            if root.get("reason").and_then(|r| r.as_str()) == Some(reason.as_str()) {
                root["reason"] = json!(placed);
            }
        }
    }
    rebuilt(serde_json::to_vec(&v).unwrap_or_default().into())
}

async fn search_answer(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let mut body = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if !body.is_object() {
        return err(StatusCode::BAD_REQUEST, "parsing_exception", "body must be an object");
    }
    fold_params_into_body(&mut body, &p);
    // A name with a cluster in front of it -- `remote:index` -- is another
    // cluster's to answer. Each cluster reduces its own shards and this one
    // puts the answers together, which is the coordinating half of a
    // cross-cluster search.
    let split = crate::api::split_expression(&store, &expr);
    if !split.remote.is_empty() {
        return across_clusters(&store, split, &expr, body, &p).await;
    }
    // `stats: [name]` tags the query so _stats can report per-group counts
    if let Some(groups) = body.get("stats").and_then(|v| v.as_array()) {
        let names: Vec<String> =
            groups.iter().filter_map(|g| g.as_str().map(|s| s.to_string())).collect();
        for n in store.resolve(&expr) {
            if let Some(st) = store.get(&n) {
                let g = st.read();
                let mut m = g.search_groups.write();
                for name in &names {
                    *m.entry(name.clone()).or_insert(0) += 1;
                }
            }
        }
    }
    note_fielddata(&store, &expr, &body);
    if let Some(r) = check_scroll(&store, &expr, &body, &p) {
        return r;
    }
    let scrolling = p.contains_key("scroll");
    // a search pipeline may change the request before it runs and the
    // answer after
    let pipeline = match crate::search::pipeline::resolve(&store, &expr, &body, &p) {
        Ok(pl) => pl,
        Err(e) => return pipeline_failure(&e),
    };
    crate::search::pipeline::strip(&mut body);
    let mut request_context = serde_json::Map::new();
    if let Some(pl) = &pipeline
        && let Err(e) = crate::search::pipeline::before(&store, pl, &mut body, &mut request_context)
    {
        return pipeline_failure(&e);
    }
    // A scroll walks the index in an order of its own so that each batch can
    // carry on from where the last one ended. Without one it would have to
    // count from the beginning every time, which costs more with every batch.
    // `_doc` is not that order: it numbers documents inside a segment, so the
    // same number comes back once per segment and a cursor built on it would
    // step over whole segments. `_seq` is the write order of the index as a
    // whole, so a batch can say where it ended and be believed.
    let implicit_sort = scrolling && body.get("sort").is_none() && !p.contains_key("sort");
    if implicit_sort {
        body["sort"] = json!([{"_seq": "asc"}]);
    }
    // A search that asks for no documents over an index nothing has touched
    // is the same question with the same answer every time it is asked, and a
    // dashboard asks it once per panel per viewer. What was worked out before
    // is handed back, and the time it took to hand back is the time it took.
    let targets = store.resolve(&expr);
    let cache_key = crate::search::request_cache::cacheable(&store, &targets, &body, &p)
        .then(|| crate::search::request_cache::key(&store, &expr, &targets, &body, &p));
    if let Some(k) = &cache_key {
        let found = store.request_cache.get(k);
        let counter: fn(&IdxState) -> &std::sync::atomic::AtomicU64 = match found {
            Some(_) => |st| &st.request_cache_hit,
            None => |st| &st.request_cache_miss,
        };
        for name in &targets {
            if let Some(st) = store.get(name) {
                counter(&st.read()).fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if let Some(mut hit) = found {
            hit["took"] = json!(0);
            return respond(&p, hit);
        }
    }
    match crate::search::run(&store, &expr, &body, &p) {
        Ok(out) => {
            let n = out.hits.len();
            let mut env = crate::search::envelope(out, &body, &p);
            if let Some(pl) = &pipeline
                && let Err(e) = crate::search::pipeline::after(pl, &mut env, &request_context)
            {
                return pipeline_failure(&e);
            }
            if scrolling {
                let size = scroll_size(&body, &p);
                // where this batch ended, so the next one starts there
                // one index numbers its writes for itself, so a cursor over
                // `_seq` only names one document while the scroll reads a
                // single index; across several it would name one per index and
                // the batch after it would be short. Those count from the
                // beginning instead.
                let cursor = (implicit_sort && store.resolve(&expr).len() == 1)
                    .then(|| last_sort_of(&env))
                    .flatten();
                if implicit_sort {
                    strip_sort(&mut env);
                }
                let keep = p
                    .get("scroll")
                    .and_then(|v| crate::api::shared::parse_keep_alive(v))
                    .map(|s| s * 1000)
                    .unwrap_or(crate::store::DEFAULT_KEEP_ALIVE_MS);
                let id = store.open_scroll(
                    &expr,
                    &body,
                    n.max(size).min(size.max(n)),
                    cursor,
                    implicit_sort,
                    keep,
                );
                env["_scroll_id"] = json!(id);
            }
            if let Some(k) = cache_key {
                store.request_cache.put(k, env.clone());
            }
            respond(&p, env)
        }
        Err(r) => r,
    }
}

/// How many searches one `_msearch` body may ask for. Each is a full search
/// run one after another on the thread answering the request.
const MOST_SUB_SEARCHES: usize = 1_000;

pub async fn msearch(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let default_index = index.map(|Path(i)| i).unwrap_or_default();
    // request-level parameters are validated once, before any sub-search runs
    if let Err(r) = crate::search::validate_params(&json!({}), &p) {
        return r;
    }
    let mut responses = Vec::new();
    let mut lines = body.lines().filter(|l| !l.trim().is_empty());
    let mut asked = 0usize;
    while let Some(header_line) = lines.next() {
        asked += 1;
        // one search per pair, and a body that has more searches than this
        // is a body that asks for more work than a request may
        if asked > MOST_SUB_SEARCHES {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!(
                    "Batch size is too large, size must be less than or equal to: \
                     [{MOST_SUB_SEARCHES}]"
                ),
            );
        }
        // A header that is not an object is not a header. Reading it as `{}`
        // left the search with no index, and no index is every index: a
        // truncated or misquoted header turned one index's search into the
        // whole cluster's, and the caller read the hits as that index's.
        let header: Value = match serde_json::from_str(header_line) {
            Ok(v @ Value::Object(_)) => v,
            _ => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "parsing_exception",
                    format!("Malformed action/metadata line [{asked}], expected an object"),
                );
            }
        };
        // and a header with no body after it is a request that was cut: the
        // answer used to come back one shorter than the searches sent, which
        // a caller pairs by position
        let Some(body_line) = lines.next() else {
            return err(
                StatusCode::BAD_REQUEST,
                "parsing_exception",
                format!("Validation Failed: 1: no request body for action line [{asked}];"),
            );
        };
        let mut req: Value = match serde_json::from_str(body_line) {
            Ok(v) => v,
            Err(e) => {
                return err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string());
            }
        };
        let expr = header
            .get("index")
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Array(a) => {
                    Some(a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(","))
                }
                _ => None,
            })
            .unwrap_or_else(|| default_index.clone());
        fold_params_into_body(&mut req, &p);
        if let Some(hdr) = header.as_object() {
            for (k, v) in hdr {
                if k != "index" && req.get(k).is_none() {
                    req[k.clone()] = v.clone();
                }
            }
        }
        // a bad parameter in any sub-request fails the whole msearch
        if let Some(why) = crate::security::item_refusal_audited(
            &store,
            &["indices:data/read/search"],
            &expr,
            &crate::security::layer::indices_for_expr(&store, &expr),
            || Some(req.to_string()),
        ) {
            responses.push(json!({"error": crate::security::item_error(&why), "status": 403}));
            continue;
        }
        if let Err(r) = crate::search::validate_params(&req, &p) {
            return r;
        }
        match crate::search::run(&store, &expr, &req, &p) {
            Ok(out) => {
                let mut env = crate::search::envelope(out, &req, &p);
                env["status"] = json!(200);
                responses.push(env);
            }
            Err(refusal) => {
                // one search of a multi search failing is that search's
                // answer, not the whole request's -- and it is the answer it
                // would have given on its own, rather than a missing index
                // whatever went wrong
                let status = refusal.status().as_u16();
                let body = axum::body::to_bytes(refusal.into_body(), 64 * 1024 * 1024)
                    .await
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
                match body.as_ref().and_then(|b| b.get("error")).cloned() {
                    Some(mut error) => {
                        add_stack_trace(&mut error, &p, "msearch");
                        responses.push(json!({"error": error, "status": status}));
                    }
                    None => {
                        let reason = format!("no such index [{expr}]");
                        let mut error = json!({
                            "type": "index_not_found_exception",
                            "reason": reason,
                            "index": expr,
                            "resource.type": "index_or_alias",
                            "resource.id": expr,
                            "index_uuid": "_na_",
                            "root_cause": [{
                                "type": "index_not_found_exception",
                                "reason": reason,
                                "index": expr,
                                "resource.type": "index_or_alias",
                                "resource.id": expr,
                                "index_uuid": "_na_"
                            }]
                        });
                        add_stack_trace(&mut error, &p, "msearch");
                        responses.push(json!({"error": error, "status": 404}));
                    }
                }
            }
        }
    }
    respond(&p, json!({"took": 1, "responses": responses}))
}

pub async fn search_shards(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let expr = index.map(|Path(i)| i);
    // the indices as the cluster knows them, and this node's own besides
    let published = crate::cluster::current_state();
    let mut names = match expr.as_deref() {
        Some(e) => store.resolve(e),
        None => store.names(),
    };
    for n in published.indices.keys() {
        let wanted = match expr.as_deref() {
            None => true,
            Some(e) => e.split(',').map(|x| x.trim()).any(|part| {
                part == n
                    || part == "_all"
                    || (part.contains('*') && crate::store::glob_match(part, n))
            }),
        };
        if wanted && !names.contains(n) {
            names.push(n.clone());
        }
    }
    // an index reached through an alias reports which alias led to it, since
    // an alias may carry a filter the caller needs to know about
    let mut via: Vec<String> = Vec::new();
    if let Some(e) = expr.as_deref() {
        // every alias the expression names, whether outright or by pattern
        let all: Vec<String> = store
            .names()
            .iter()
            .filter_map(|n| store.get(n))
            .flat_map(|st| st.read().aliases.keys().cloned().collect::<Vec<_>>())
            .collect();
        for part in e.split(',').map(|n| n.trim()) {
            if part.contains('*') {
                for a in &all {
                    if crate::store::glob_match(part, a) && !via.contains(a) {
                        via.push(a.clone());
                    }
                }
            } else if store.is_alias(part) && !via.contains(&part.to_string()) {
                via.push(part.to_string());
            }
        }
        via.sort();
        via.dedup();
    }
    // an index is listed shard by shard; a slice takes every `max`th of them,
    // which is how several readers divide one index between them
    let slice = body.get("slice");
    let slice_id = slice.and_then(|s| s.get("id")).and_then(|v| v.as_u64()).unwrap_or(0);
    let slice_max = slice.and_then(|s| s.get("max")).and_then(|v| v.as_u64()).unwrap_or(1).max(1);
    // `preference: _shards:...` narrows to the shards it names before
    // anything else looks at the list
    let preferred: Option<Vec<u64>> = p.get("preference").and_then(|v| {
        v.strip_prefix("_shards:")
            .map(|list| list.split(',').filter_map(|s| s.trim().parse::<u64>().ok()).collect())
    });
    // each shard as the manager placed its copies; an index the manager has
    // not placed yet is this node's alone
    let live = crate::cluster::current_state();
    let me = crate::cluster::identity();
    let mut listed: Vec<(String, u64)> = Vec::new();
    for n in &names {
        let count = live
            .indices
            .get(n)
            .map(|m| m.number_of_shards as u64)
            .or_else(|| {
                store
                    .get(n)
                    .map(|st| st.read().numeric_setting("number_of_shards").unwrap_or(1).max(1))
            })
            .unwrap_or(1);
        for shard in 0..count {
            if preferred.as_ref().map(|w| !w.contains(&shard)).unwrap_or(false) {
                continue;
            }
            listed.push((n.clone(), shard));
        }
    }
    // a slice takes every `max`th of what is left, counted by position rather
    // than by shard number -- the two differ once a preference has narrowed it
    let mut nodes_used: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let shards: Vec<Value> = listed
        .into_iter()
        .enumerate()
        .filter(|(i, _)| slice.is_none() || *i as u64 % slice_max == slice_id)
        .map(|(_, (n, shard))| {
            let copies: Vec<Value> = live
                .routing
                .shards_of(&n)
                .filter(|c| c.shard as u64 == shard && c.node.is_some())
                .map(|c| {
                    if let Some(nd) = &c.node {
                        nodes_used.insert(nd.as_str().to_string());
                    }
                    c.to_json()
                })
                .collect();
            if copies.is_empty() {
                nodes_used.insert(me.id.as_str().to_string());
                json!([{
                    "state": "STARTED", "primary": true, "node": me.id.as_str(),
                    "relocating_node": null, "shard": shard, "index": n,
                    "allocation_id": {"id": "_na_"}
                }])
            } else {
                Value::Array(copies)
            }
        })
        .collect();
    let mut nodes = serde_json::Map::new();
    for id in nodes_used {
        let entry = match live.nodes.get(&crate::cluster::NodeId(id.clone())) {
            Some(nd) => json!({"name": nd.name, "ephemeral_id": nd.ephemeral_id.as_str(),
                "transport_address": nd.transport_address, "attributes": nd.attributes}),
            None => json!({"name": me.name, "ephemeral_id": me.ephemeral_id.as_str(),
                "transport_address": me.transport_address, "attributes": {}}),
        };
        nodes.insert(id, entry);
    }
    respond(
        &p,
        json!({
            "nodes": nodes,
            "indices": names
                .iter()
                .map(|n| {
                    let mut entry = json!({});
                    let own: Vec<String> = via
                        .iter()
                        .filter(|a| store.resolve(a).iter().any(|r| r == n))
                        .cloned()
                        .collect();
                    if !own.is_empty() {
                        // an alias may narrow what the index shows, and a caller
                        // routing its own search needs that filter
                        if let Some(st) = store.get(n) {
                            let g = st.read();
                            // several aliases reaching the same index each narrow
                            // it, and a document matching any of them is visible
                            let filters: Vec<Value> = own
                                .iter()
                                .filter_map(|a| g.aliases.get(a).and_then(|d| d.get("filter")))
                                .map(expand_filter)
                                .collect();
                            // an alias with no filter of its own opens the index
                            // up again, so there is nothing left to narrow
                            if filters.len() == own.len() {
                                match filters.len() {
                                    0 => {}
                                    1 => entry["filter"] = filters[0].clone(),
                                    // the bool a combined filter becomes carries
                                    // the defaults a bool query is built with
                                    _ => {
                                        entry["filter"] = json!({"bool": {
                                            "should": filters,
                                            "adjust_pure_negative": true,
                                            "boost": 1.0,
                                        }})
                                    }
                                }
                            }
                        }
                        entry["aliases"] = json!(own);
                    }
                    (n.clone(), entry)
                })
                .collect::<serde_json::Map<_, _>>(),
            "shards": shards,
        }),
    )
}

/// A search pipeline's failure as a response.
pub(crate) fn pipeline_failure(e: &crate::search::pipeline::PipelineError) -> Response {
    let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::BAD_REQUEST);
    (status, axum::Json(json!({"error": e.body(), "status": e.status()}))).into_response()
}

/// The sort values of the last document a page returned, which is where the
/// next page begins.
pub(crate) fn last_sort_of(env: &Value) -> Option<Vec<Value>> {
    env.pointer("/hits/hits")?.as_array()?.last()?.get("sort")?.as_array().map(|a| a.to_vec())
}

/// Take the sort values back off the hits, for an order the caller did not
/// ask for and should not be told about.
pub(crate) fn strip_sort(env: &mut Value) {
    if let Some(hits) = env.pointer_mut("/hits/hits").and_then(|h| h.as_array_mut()) {
        for hit in hits {
            if let Some(o) = hit.as_object_mut() {
                o.remove("sort");
            }
        }
    }
}

/// How many shards a cross-cluster pre-filter skipped, where it is one
/// pre-filter.
///
/// With `ccs_minimize_roundtrips: false` the coordinator sees every shard of
/// every cluster and skips across all of them at once, keeping one so there
/// is an answer to give. Each cluster keeps one of its own when it is asked
/// alone, so their counts added up said a one-shard remote that could not
/// match was not skipped. A cluster that matched nothing could skip all its
/// shards; the rule of keeping one is applied once, to the whole.
fn prefiltered_skips(answers: &[(String, Value)], body: &Value, p: &Params) -> Option<u64> {
    let minimized = p.get("ccs_minimize_roundtrips").map(|v| v != "false").unwrap_or(true);
    let aggregates = body.get("aggs").is_some() || body.get("aggregations").is_some();
    if minimized
        || !p.contains_key("pre_filter_shard_size")
        || body.get("query").is_none()
        || aggregates
    {
        return None;
    }
    let (mut total, mut skippable) = (0u64, 0u64);
    for (_, answer) in answers {
        let shards = answer.pointer("/_shards/total").and_then(|v| v.as_u64()).unwrap_or(0);
        let matched = answer
            .pointer("/hits/total/value")
            .or_else(|| answer.pointer("/hits/total"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let own = answer.pointer("/_shards/skipped").and_then(|v| v.as_u64()).unwrap_or(0);
        total += shards;
        skippable += if matched == 0 { shards } else { own };
    }
    Some(skippable.min(total.saturating_sub(1)))
}

/// A remote scroll's id, as this cluster hands it out: which cluster holds
/// the scroll, and the id that cluster gave it.
const REMOTE_SCROLL: &str = "ccs~";

/// A scroll over one remote cluster's indices.
///
/// The remote keeps the scroll; this cluster keeps nothing but its name in
/// the id, so the next batch is asked of the cluster that holds it. A scroll
/// over indices of this cluster and another at once is refused rather than
/// answered as though it were a scroll over one of them.
fn scroll_across_clusters(
    store: &Store,
    split: crate::api::Split,
    body: Value,
    p: &Params,
) -> Response {
    if !split.local.is_empty() || split.remote.len() != 1 {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "a scroll across clusters may name the indices of one remote cluster only",
        );
    }
    let Some((name, indices)) = split.remote.into_iter().next() else {
        return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", "no cluster named");
    };
    let known = crate::api::remotes(store);
    let Some(remote) = known.get(&name) else {
        return crate::api::shared::no_such_index(&format!("{name}:{indices}"));
    };
    let query = p
        .iter()
        .filter(|(k, _)| {
            matches!(
                k.as_str().unwrap_or(""),
                "scroll"
                    | "sort"
                    | "size"
                    | "from"
                    | "rest_total_hits_as_int"
                    | "typed_keys"
                    | "track_total_hits"
                    | "_source"
            )
        })
        .map(|(k, v)| format!("{k}={}", encode_component(v)))
        .collect::<Vec<_>>()
        .join("&");
    let path = format!("/{indices}/_search");
    let answered =
        tokio::task::block_in_place(|| crate::api::ask(remote, "POST", &path, &query, Some(&body)));
    let (status, mut value) = match answered {
        Ok(found) => found,
        Err(why) => return err(StatusCode::BAD_GATEWAY, "connect_transport_exception", why),
    };
    if status >= 300 {
        let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
        return (code, axum::Json(value)).into_response();
    }
    name_remote_hits(&mut value, &name);
    if let Some(id) = value.get("_scroll_id").and_then(|v| v.as_str()).map(String::from) {
        value["_scroll_id"] = json!(format!("{REMOTE_SCROLL}{name}~{id}"));
    }
    value["_clusters"] = json!({"total": 1, "successful": 1, "skipped": 0});
    respond(p, value)
}

/// A remote's hits, with its name in front of each index.
fn name_remote_hits(value: &mut Value, cluster: &str) {
    if let Some(hits) = value.pointer_mut("/hits/hits").and_then(|h| h.as_array_mut()) {
        for hit in hits {
            if let Some(index) = hit.get("_index").and_then(|v| v.as_str()).map(String::from) {
                hit["_index"] = json!(format!("{cluster}:{index}"));
            }
        }
    }
}

/// A value as it may stand in a URL.
fn encode_component(v: &str) -> String {
    v.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' | b',' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// The next batch of a scroll a remote cluster holds, if the id names one.
pub(crate) fn remote_scroll(
    store: &Store,
    id: &str,
    keep: Option<&str>,
    p: &Params,
) -> Option<Response> {
    let (name, theirs) = id.strip_prefix(REMOTE_SCROLL)?.split_once('~')?;
    let known = crate::api::remotes(store);
    let Some(remote) = known.get(name) else {
        return Some(err(
            StatusCode::NOT_FOUND,
            "search_context_missing_exception",
            format!("No search context found for id [{id}]"),
        ));
    };
    let query = if p.get("rest_total_hits_as_int").map(|v| v == "true").unwrap_or(false) {
        "rest_total_hits_as_int=true"
    } else {
        ""
    };
    let body = json!({"scroll_id": theirs, "scroll": keep.unwrap_or("1m")});
    let answered = tokio::task::block_in_place(|| {
        crate::api::ask(remote, "POST", "/_search/scroll", query, Some(&body))
    });
    let (status, mut value) = match answered {
        Ok(found) => found,
        Err(why) => return Some(err(StatusCode::BAD_GATEWAY, "connect_transport_exception", why)),
    };
    if status >= 300 {
        let code = StatusCode::from_u16(status).unwrap_or(StatusCode::NOT_FOUND);
        return Some((code, axum::Json(value)).into_response());
    }
    name_remote_hits(&mut value, name);
    value["_scroll_id"] = json!(id);
    if let Some(o) = value.as_object_mut() {
        o.remove("_clusters");
    }
    Some(respond(p, value))
}

/// A remote scroll let go of, if the id names one; whether it was there.
pub(crate) fn clear_remote_scroll(store: &Store, id: &str) -> bool {
    let Some((name, theirs)) = id.strip_prefix(REMOTE_SCROLL).and_then(|r| r.split_once('~'))
    else {
        return false;
    };
    let known = crate::api::remotes(store);
    let Some(remote) = known.get(name) else { return false };
    let path = format!("/_search/scroll/{}", encode_component(theirs));
    matches!(
        tokio::task::block_in_place(|| crate::api::ask(remote, "DELETE", &path, "", None)),
        Ok((200, _))
    )
}

/// A search that names another cluster: ask each of them, and put the answers
/// together.
async fn across_clusters(
    store: &Store,
    split: crate::api::Split,
    asked: &str,
    body: Value,
    p: &Params,
) -> Response {
    if p.contains_key("scroll") {
        return scroll_across_clusters(store, split, body, p);
    }
    let known = crate::api::remotes(store);
    let size = body.get("size").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
    let from = body.get("from").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    // each cluster answers the whole question, so each is asked for enough
    // hits to be able to supply the whole page on its own
    let mut asked_body = body.clone();
    asked_body["size"] = json!(size + from);
    asked_body["from"] = json!(0);
    // An average of averages is not an average: each cluster is asked for the
    // sum and the count behind its `avg`, and the average is worked out once
    // the counts are together. The answer used to keep the first cluster's
    // average and report it for both.
    // An aggregation over `_index` keys its buckets by index name, and a
    // remote's index names carry the cluster in front of them.
    let mut plan = crate::api::plan_across_clusters(&mut asked_body);
    // a sort written in the URL runs the way it says: `field:desc`
    if body.get("sort").is_none()
        && let Some(sort) = p.get("sort")
    {
        plan.descending = sort.split(',').map(|s| s.trim().ends_with(":desc")).collect();
    }
    let mut answers: Vec<(String, Value)> = Vec::new();
    let mut skipped = 0usize;
    let mut successful = 0usize;

    // this cluster's own share, where the expression named any of it
    if split.local.is_empty() {
        answers.push((
            String::new(),
            json!({"took": 0, "timed_out": false,
                   "_shards": {"total": 0, "successful": 0, "skipped": 0, "failed": 0},
                   "hits": {"total": {"value": 0, "relation": "eq"},
                            "max_score": Value::Null, "hits": []}}),
        ));
    } else {
        match crate::search::run(store, &split.local, &asked_body, p) {
            Ok(out) => {
                successful += 1;
                answers.push((String::new(), crate::search::envelope(out, &asked_body, p)));
            }
            Err(r) => return r,
        }
    }

    let query = p
        .iter()
        .filter(|(k, _)| {
            matches!(
                k.as_str(),
                Some("rest_total_hits_as_int")
                    | Some("typed_keys")
                    | Some("ignore_unavailable")
                    | Some("pre_filter_shard_size")
                    | Some("sort")
            )
        })
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    for (name, indices) in split.remote {
        let Some(remote) = known.get(&name) else { continue };
        let path = format!("/{indices}/_search");
        // A remote's documents are `<cluster>:<index>` to the caller and
        // `<index>` to the remote itself: a clause on `_index` naming
        // `remote:x` is `x` there, and one naming a bare `x` names an index of
        // this cluster, which no document over there is in.
        let mut theirs = asked_body.clone();
        if let Some(q) = theirs.get_mut("query") {
            crate::api::localize_index_clauses(q, &name);
        }
        let answered = tokio::task::block_in_place(|| {
            crate::api::ask(remote, "POST", &path, &query, Some(&theirs))
        });
        match answered {
            Ok((status, value)) if status < 300 => {
                successful += 1;
                answers.push((name, value));
            }
            // a cluster that answers with a refusal answers for itself: the
            // caller is told, unless they said to skip it
            Ok((status, value)) => {
                if remote.skip_unavailable {
                    skipped += 1;
                    continue;
                }
                let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
                // passed on as the remote said it: OpenSearch does not put the
                // cluster's name in front of the reason
                return (code, axum::Json(value)).into_response();
            }
            Err(why) => {
                if remote.skip_unavailable {
                    skipped += 1;
                    continue;
                }
                return err(StatusCode::BAD_GATEWAY, "connect_transport_exception", why);
            }
        }
    }
    let total_clusters = successful + skipped;
    let prefiltered = prefiltered_skips(&answers, &body, p);
    let mut merged = crate::api::merge_answers(answers, size, from, &plan);
    if let Some(n) = prefiltered {
        merged["_shards"]["skipped"] = json!(n);
    }
    merged["_clusters"] = json!({
        "total": total_clusters, "successful": successful, "skipped": skipped,
    });
    // every cluster reduced its own shards and this reduction joined them --
    // a phase that only exists when there was more than one to join
    // with `ccs_minimize_roundtrips: false` the coordinator reduces the
    // shards itself in one phase, and there is no per-cluster phase to count
    let minimized = p.get("ccs_minimize_roundtrips").map(|v| v != "false").unwrap_or(true);
    if total_clusters > 1 && minimized {
        merged["num_reduce_phases"] = json!(total_clusters + 1);
    }
    // the total in the form the caller asked for it
    if p.get("rest_total_hits_as_int").map(|v| v == "true").unwrap_or(false)
        && let Some(v) = merged.pointer("/hits/total/value").cloned()
    {
        merged["hits"]["total"] = v;
    }
    let _ = asked;
    respond(p, merged)
}
