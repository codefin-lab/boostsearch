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
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let mut body = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if !body.is_object() {
        return err(StatusCode::BAD_REQUEST, "parsing_exception", "body must be an object");
    }
    fold_params_into_body(&mut body, &p);
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
                let id = store.open_scroll(
                    &expr,
                    &body,
                    n.max(size).min(size.max(n)),
                    cursor,
                    implicit_sort,
                );
                env["_scroll_id"] = json!(id);
            }
            respond(&p, env)
        }
        Err(r) => r,
    }
}

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
    while let Some(header_line) = lines.next() {
        let header: Value = serde_json::from_str(header_line).unwrap_or(json!({}));
        let Some(body_line) = lines.next() else { break };
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
        if let Some(why) = crate::security::item_refusal(
            &store,
            &["indices:data/read/search"],
            &crate::security::layer::indices_for_expr(&store, &expr),
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
            Err(_) => {
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
    env.pointer("/hits/hits")?
        .as_array()?
        .last()?
        .get("sort")?
        .as_array()
        .map(|a| a.to_vec())
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
