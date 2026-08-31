//! The endpoints that search: `_search` and everything that stands beside it.

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
    respond(&p, json!({
        "pit_id": id,
        "_shards": shards_over(&store, &names),
        "creation_time": 0,
    }))
}

pub async fn get_all_pits(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    let pits: Vec<Value> = store
        .all_pits()
        .into_iter()
        .map(|(id, st)| json!({
            "pit_id": id, "creation_time": 0, "keep_alive": st.keep_alive_ms,
        }))
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

pub(crate) fn check_scroll(store: &Store, expr: &str, body: &Value, p: &Params) -> Option<Response> {
    let Some(keep) = p.get("scroll") else { return None };
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
    if let (Some(limit), Some(want)) = (limit, parse_keep_alive(keep)) {
        if want > limit {
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
    }
    None
}

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
    match crate::search::run(&store, &expr, &body, &p) {
        Ok(out) => {
            let n = out.hits.len();
            let mut env = crate::search::envelope(out, &body, &p);
            if scrolling {
                let size = scroll_size(&body, &p);
                let id = store.open_scroll(&expr, &body, n.max(size).min(size.max(n)));
                // the cursor starts after what this response already returned
                store.advance_scroll(&id, 0);
                env["_scroll_id"] = json!(id);
            }
            respond(&p, env)
        }
        Err(r) => r,
    }
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
    let asked = body.get("scroll").and_then(|v| v.as_str()).map(|s| s.to_string())
        .or_else(|| p.get("scroll").cloned());
    if let (Some(keep), Some(limit)) = (
        asked.as_deref(),
        store
            .cluster_setting("search.max_keep_alive")
            .and_then(|v| v.as_str().map(|s| s.to_string())),
    ) {
        if let (Some(want), Some(cap)) = (parse_keep_alive(keep), parse_keep_alive(&limit)) {
            if want > cap {
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
        }
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
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect(),
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
    if freed == 0 {
        return err(
            StatusCode::NOT_FOUND,
            "search_context_missing_exception",
            "No search context found",
        );
    }
    respond(&p, json!({"succeeded": true, "num_freed": freed}))
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
                Value::Array(a) => Some(
                    a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(","),
                ),
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

pub(crate) fn caps_for(kind: &str) -> Value {
    // a container holds no values of its own: nothing to search it for, and
    // nothing to aggregate over
    let container = matches!(kind, "object" | "nested");
    let aggregatable = kind != "text" && !container;
    let searchable = !container;
    json!({"type": kind, "searchable": searchable, "aggregatable": aggregatable})
}

pub async fn field_caps(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_else(|| "_all".into());
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" {
        return no_such_index(&expr);
    }
    let patterns: Vec<String> = p
        .get("fields")
        .map(|f| f.split(',').map(|s| s.trim().to_string()).collect())
        .or_else(|| {
            body.get("fields").and_then(|f| f.as_array()).map(|a| {
                a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()
            })
        })
        .unwrap_or_else(|| vec!["*".into()]);

    // an index_filter drops indices whose documents don't match
    let index_filter = body.get("index_filter").cloned();
    let mut kept: Vec<String> = Vec::new();
    for n in &targets {
        if let Some(f) = &index_filter {
            let probe = json!({"query": f, "size": 0});
            let hit = crate::search::run(&store, n, &probe, &Params::new())
                .map(|o| o.total > 0)
                .unwrap_or(false);
            if !hit {
                continue;
            }
        }
        kept.push(n.clone());
    }

    let mut fields: serde_json::Map<String, Value> = serde_json::Map::new();
    for n in &kept {
        let Some(st) = store.get(n) else { continue };
        let g = st.read();
        for (name, kind) in g.all_field_types() {
            if !patterns.iter().any(|pat| {
                pat == "*" || *pat == name || crate::store::wildcard_to_regex(pat).is_match(&name)
            }) {
                continue;
            }
            let kinds: Vec<String> = vec![kind.clone()];
            let meta = g
                .mapping
                .raw
                .pointer(&format!("/properties/{}/meta", name.replace('.', "/properties/")))
                .cloned();
            // a field with no doc values cannot be aggregated over
            let has_doc_values = g
                .mapping
                .raw
                .pointer(&format!(
                    "/properties/{}/doc_values",
                    name.replace('.', "/properties/")
                ))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            // a field the mapping says not to index cannot be searched for
            let indexed = g
                .mapping
                .raw
                .pointer(&format!("/properties/{}/index", name.replace('.', "/properties/")))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            for kind in kinds {
            let entry = fields.entry(name.clone()).or_insert_with(|| json!({}));
            let slot = entry_of(entry, &kind, || caps_for(&kind));
            if !has_doc_values {
                let where_not = entry_of(slot, "__unaggregatable", || json!([]));
                if let Some(a) = where_not.as_array_mut() {
                    a.push(json!(n));
                }
            }
            if !indexed {
                // remember where it is not searchable; if that is everywhere,
                // the field simply is not searchable
                let where_not = entry_of(slot, "__unsearchable", || json!([]));
                if let Some(a) = where_not.as_array_mut() {
                    a.push(json!(n));
                }
            }
            if let Some(m) = meta.clone().and_then(|m| m.as_object().cloned()) {
                let dst = entry_of(slot, "meta", || json!({}));
                for (mk, mv) in m {
                    let list = entry_of(dst, &mk, || json!([]));
                    if let Some(a) = list.as_array_mut() {
                        if !a.contains(&mv) {
                            a.push(mv);
                        }
                    }
                }
            }
            // a type seen in only some indices lists the ones it came from
            let indices = entry_of(slot, "__indices", || json!([]));
            if let Some(a) = indices.as_array_mut() {
                a.push(json!(n));
            }
            }
        }
    }

    // `include_unmapped` names the indices a field is missing from, which
    // means the ones it is present in have to be named too
    let unmapped = flag(&p, "include_unmapped");
    if unmapped {
        let mut extra: Vec<(String, Value)> = Vec::new();
        for (name, per_type) in fields.iter() {
            let mut has: Vec<String> = Vec::new();
            for (_, v) in per_type.as_object().into_iter().flatten() {
                for i in v.get("__indices").and_then(|i| i.as_array()).into_iter().flatten() {
                    if let Some(s) = i.as_str() {
                        has.push(s.to_string());
                    }
                }
            }
            let missing: Vec<String> =
                kept.iter().filter(|n| !has.contains(n)).cloned().collect();
            if !missing.is_empty() {
                extra.push((name.clone(), json!(missing)));
            }
        }
        for (name, missing) in extra {
            if let Some(o) = fields.get_mut(&name).and_then(|v| v.as_object_mut()) {
                // the mapped types now have to say where they are, since one
                // of the entries says where the field is not
                for (_, v) in o.iter_mut() {
                    if let Some(i) = v.get("__indices").cloned() {
                        v["indices"] = i;
                    }
                }
                o.insert(
                    "unmapped".to_string(),
                    json!({
                        "type": "unmapped",
                        "searchable": false,
                        "aggregatable": false,
                        "indices": missing,
                    }),
                );
            }
        }
    }

    // only report `indices` on a field whose type is not uniform
    for (_, per_type) in fields.iter_mut() {
        let type_count = per_type.as_object().map(|o| o.len()).unwrap_or(0);
        if let Some(o) = per_type.as_object_mut() {
            for (_, v) in o.iter_mut() {
                let Some(o) = v.as_object_mut() else { continue };
                let idx = o.remove("__indices");
                let unsearchable = o.remove("__unsearchable");
                let unaggregatable = o.remove("__unaggregatable");
                if let (Some(Value::Array(no)), Some(Value::Array(all))) =
                    (unaggregatable, idx.clone())
                {
                    if no.len() == all.len() {
                        v["aggregatable"] = json!(false);
                    } else if !no.is_empty() {
                        v["aggregatable"] = json!(false);
                        v["non_aggregatable_indices"] = json!(no);
                    }
                }
                // searchable in some indices and not others: say which
                if let (Some(Value::Array(no)), Some(Value::Array(all))) =
                    (unsearchable.clone(), idx.clone())
                {
                    if no.len() == all.len() {
                        v["searchable"] = json!(false);
                    } else if !no.is_empty() {
                        v["searchable"] = json!(false);
                        v["non_searchable_indices"] = json!(no);
                    }
                }
                // with `include_unmapped`, a field present everywhere still
                // needs no listing: there is nothing it is missing from
                let partly = type_count > 1;
                if partly {
                    if let Some(i) = idx {
                        v["indices"] = i;
                    }
                }
            }
        }
    }

    respond(&p, json!({"indices": kept, "fields": Value::Object(fields)}))
}

/// A filter written the short way, spelled out.
///
/// `{"term": {"field": "value"}}` and `{"term": {"field": {"value": "value"}}}`
/// mean the same thing; the long form is what a filter is reported as, since
/// it is where the boost would go.
pub(crate) fn expand_filter(f: &Value) -> Value {
    let Some(o) = f.as_object() else { return f.clone() };
    let mut out = serde_json::Map::new();
    for (kind, body) in o {
        if !matches!(kind.as_str(), "term" | "prefix" | "wildcard" | "regexp" | "fuzzy") {
            out.insert(kind.clone(), body.clone());
            continue;
        }
        let Some(fields) = body.as_object() else {
            out.insert(kind.clone(), body.clone());
            continue;
        };
        let mut spelled = serde_json::Map::new();
        for (field, v) in fields {
            let long = match v {
                Value::Object(_) => v.clone(),
                other => json!({"value": other, "boost": 1.0}),
            };
            spelled.insert(field.clone(), long);
        }
        out.insert(kind.clone(), Value::Object(spelled));
    }
    Value::Object(out)
}

pub async fn search_shards(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let expr = index.map(|Path(i)| i);
    let names = match expr.as_deref() {
        Some(e) => store.resolve(e),
        None => store.names(),
    };
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
        v.strip_prefix("_shards:").map(|list| {
            list.split(',').filter_map(|s| s.trim().parse::<u64>().ok()).collect()
        })
    });
    let mut listed: Vec<(String, u64)> = Vec::new();
    for n in &names {
        let count = store
            .get(n)
            .map(|st| st.read().numeric_setting("number_of_shards").unwrap_or(1).max(1))
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
    let shards: Vec<Value> = listed
        .into_iter()
        .enumerate()
        .filter(|(i, _)| slice.is_none() || *i as u64 % slice_max == slice_id)
        .map(|(_, (n, shard))| json!([{
            "state": "STARTED", "primary": true, "node": "node-0",
            "relocating_node": null, "shard": shard, "index": n,
            "allocation_id": {"id": "_na_"}
        }]))
        .collect();
    respond(&p, json!({
        "nodes": {"node-0": {"name": "boostsearch", "ephemeral_id": "_na_",
                             "transport_address": "127.0.0.1:9300", "attributes": {}}},
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
    }))
}

pub async fn validate_query(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let shards = json!({"total": 1, "successful": 1, "failed": 0});
    // a body that is not empty and does not name a query is not a query at
    // all, whatever else it contains
    let Some(query) = body.get("query").cloned() else {
        if body.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            let mut out = json!({"_shards": shards, "valid": true});
            if p.get("explain").map(|v| v != "false").unwrap_or(false) {
                let sample = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
                out["explanations"] = json!(
                    sample
                        .iter()
                        .map(|n| json!({
                            "index": n, "valid": true,
                            "explanation": describe_query(&json!({"match_all": {}})),
                        }))
                        .collect::<Vec<_>>()
                );
            }
            return respond(&p, out);
        }
        let mut out = json!({"_shards": shards, "valid": false});
        // whatever the body holds, it is not where a query goes -- said only
        // when the caller asked to be told why
        if p.get("explain").map(|v| v != "false").unwrap_or(false) {
            if let Some(first) = body.as_object().and_then(|o| o.keys().next()) {
                out["error"] = json!(format!("request does not support [{first}]"));
            }
        }
        return respond(&p, out);
    };
    let probe = json!({"query": query, "size": 0});
    // building the query against one of the targets says whether it can be
    // read at all, and `explain` asks to be told why not
    let sample = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
    if let Some(st) = sample.first().and_then(|n| store.get(n)) {
        let g = st.read();
        let ctx = crate::query::Ctx {
            fields: &g.fields,
            mapping: &g.mapping,
            index: &g.index,
            max_terms_count: g.max_terms_count(),
            max_regex_length: g.max_regex_length(),
            allow_expensive: crate::search::expensive_allowed(&store),
            observed_kinds: &g.observed_kinds,
            kinds_complete: g.kinds_complete,
            stats: &g.stats,
        };
        if let Err(e) = crate::query::build(&ctx, &query) {
            let mut out = json!({"_shards": shards, "valid": false});
            if p.get("explain").map(|v| v != "false").unwrap_or(false) {
                // the name of the index it was read against, then what went
                // wrong, which is the shape the message has
                let name = sample.first().cloned().unwrap_or_default();
                out["error"] = json!(format!("[{name}] QueryShardException[{e}]"));
            }
            return respond(&p, out);
        }
    }
    match crate::search::run(&store, &expr, &probe, &Params::new()) {
        Ok(_) => {
            let mut out = json!({"_shards": shards, "valid": true});
            if p.get("explain").map(|v| v != "false").unwrap_or(false) {
                out["explanations"] = json!(
                    sample
                        .iter()
                        .map(|n| json!({
                            "index": n, "valid": true,
                            "explanation": describe_query(&query),
                        }))
                        .collect::<Vec<_>>()
                );
            }
            respond(&p, out)
        }
        Err(_) => respond(&p, json!({"_shards": shards, "valid": false})),
    }
}

/// How a query reads once it has been rewritten, in the shape the engine
/// names its own queries.
pub(crate) fn describe_query(q: &Value) -> String {
    let Some((kind, body)) = q.as_object().and_then(|o| o.iter().next()) else {
        return "*:*".to_string();
    };
    match kind.as_str() {
        "match_all" => {
            "ApproximateScoreQuery(originalQuery=*:*, approximationQuery=Approximate(*:*))"
                .to_string()
        }
        "term" | "match" => body
            .as_object()
            .and_then(|o| o.iter().next())
            .map(|(f, v)| {
                let text = match v {
                    Value::String(s) => s.clone(),
                    Value::Object(o) => o
                        .get("value")
                        .or_else(|| o.get("query"))
                        .map(|x| x.as_str().unwrap_or_default().to_string())
                        .unwrap_or_default(),
                    other => other.to_string(),
                };
                format!("{f}:{text}")
            })
            .unwrap_or_else(|| "*:*".to_string()),
        other => other.to_string(),
    }
}

/// `_analyze` runs text through the tokenizer the query path would use.
pub async fn analyze(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let text = match body.get("text") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect(),
        _ => p.get("text").map(|t| vec![t.clone()]).unwrap_or_default(),
    };
    let analyzer = body
        .get("analyzer")
        .and_then(|v| v.as_str())
        .or_else(|| p.get("analyzer").map(|s| s.as_str()));
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let st = store.resolve(&expr).into_iter().next().and_then(|n| store.get(&n));
    // a tokenizer only splits; folding case is a filter, and naming one
    // without the other asks for the split alone
    let tokenizer_only = analyzer.is_none()
        && (body.get("tokenizer").is_some() || p.contains_key("tokenizer"));
    let mut tokens = Vec::new();
    let mut pos = 0usize;
    for t in &text {
        let parts = if tokenizer_only {
            t.split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
                .map(|w| w.to_string())
                .collect()
        } else {
            match &st {
                Some(s) => crate::query::analyze_text(&s.read().index, t, analyzer),
                None => t.split_whitespace().map(|w| w.to_lowercase()).collect(),
            }
        };
        for tok in parts {
            tokens.push(json!({
                "token": tok, "start_offset": 0, "end_offset": 0,
                "type": "<ALPHANUM>", "position": pos
            }));
            pos += 1;
        }
    }
    let cap = st
        .as_ref()
        .and_then(|s| s.read().numeric_setting("analyze.max_token_count"))
        .unwrap_or(10_000) as usize;
    if tokens.len() > cap {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "The number of tokens produced by calling _analyze has exceeded the allowed \
                 maximum of [{cap}]. This limit can be set by changing the \
                 [index.analyze.max_token_count] index level setting."
            ),
        );
    }
    // `explain` asks for the same tokens laid out by the step that produced
    // them, rather than as one flat list
    if body.get("explain").and_then(|v| v.as_bool()).unwrap_or(false)
        || p.get("explain").map(|v| v == "true").unwrap_or(false)
    {
        let named = body
            .get("tokenizer")
            .or_else(|| body.get("analyzer"))
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .or_else(|| p.get("tokenizer").or_else(|| p.get("analyzer")).cloned())
            .unwrap_or_else(|| "standard".to_string());
        let stage = json!({"name": named, "tokens": tokens.clone()});
        let detail = if tokenizer_only {
            let filters: Vec<Value> = body
                .get("filter")
                .and_then(|f| f.as_array())
                .map(|a| {
                    a.iter()
                        .map(|f| {
                            let name = match f {
                                Value::String(s) => s.clone(),
                                other => other
                                    .get("type")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("filter")
                                    .to_string(),
                            };
                            // a stop filter takes words back out, which is
                            // the whole point of naming one
                            let stop: Vec<String> = f
                                .get("stopwords")
                                .and_then(|w| w.as_array())
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                        .collect()
                                })
                                .unwrap_or_default();
                            let kept: Vec<Value> = tokens
                                .iter()
                                .filter(|t| {
                                    let text = t.get("token").and_then(|v| v.as_str()).unwrap_or("");
                                    !stop.iter().any(|w| w == text)
                                })
                                .cloned()
                                .collect();
                            json!({"name": name, "tokens": kept})
                        })
                        .collect()
                })
                .unwrap_or_default();
            json!({"custom_analyzer": true, "tokenizer": stage, "tokenfilters": filters})
        } else {
            json!({"custom_analyzer": false, "analyzer": stage})
        };
        return respond(&p, json!({"detail": detail}));
    }

    respond(&p, json!({"tokens": tokens}))
}
