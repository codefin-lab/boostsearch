//! Reading many documents by id at once.

use super::*;

pub async fn mget(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let default_index = index.map(|Path(i)| i);
    if let Some(idx) = default_index.as_deref() {
        refresh_before_read(&store, idx, &p);
    }
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };

    let mut requested: Vec<(Option<String>, Option<String>, Option<Value>)> = Vec::new();
    let mut per_doc_routing: Vec<Option<String>> = Vec::new();
    let empty_docs =
        body.get("docs").and_then(|d| d.as_array()).map(|a| a.is_empty()).unwrap_or(false)
            || body.get("ids").and_then(|d| d.as_array()).map(|a| a.is_empty()).unwrap_or(false);
    if let Some(docs) = body.get("docs").and_then(|d| d.as_array()) {
        for d in docs {
            // `routing` is a real field here; the underscored spellings are
            // the ones that were taken away
            for dep in ["_routing", "_version", "_type", "version"] {
                if d.get(dep).is_some() {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        format!("Unsupported field [{dep}] used in multi get request"),
                    );
                }
            }
        }
    }
    if empty_docs || (body.get("docs").is_none() && body.get("ids").is_none()) {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: no documents to get;",
        );
    }
    if let Some(docs) = body.get("docs").and_then(|d| d.as_array()) {
        for d in docs {
            per_doc_routing.push(d.get("routing").and_then(scalar_str));
            requested.push((
                d.get("_index").and_then(scalar_str),
                d.get("_id").and_then(scalar_str),
                d.get("_source")
                    .cloned()
                    .or_else(|| d.get("stored_fields").map(|sf| json!({"__stored": sf}))),
            ));
        }
    } else if let Some(ids) = body.get("ids").and_then(|d| d.as_array()) {
        for i in ids {
            per_doc_routing.push(None);
            requested.push((None, scalar_str(i), None));
        }
    }

    let mut docs = Vec::new();
    for (n, (idx, id, sel)) in requested.into_iter().enumerate() {
        // a routing given on the document is the one it has to be reached by
        let want_routing = per_doc_routing.get(n).cloned().flatten();
        let idx = idx.or_else(|| default_index.clone());
        let Some(idx) = idx else {
            return err(
                StatusCode::BAD_REQUEST,
                "action_request_validation_exception",
                "Validation Failed: 1: index is missing;",
            );
        };
        let Some(id) = id else {
            return err(
                StatusCode::BAD_REQUEST,
                "action_request_validation_exception",
                "Validation Failed: 1: id is missing;",
            );
        };
        let Some(st) = store.get(&idx) else {
            let reason = format!("no such index [{idx}]");
            let cause = json!({
                "type": "index_not_found_exception", "reason": reason,
                "index": idx, "resource.type": "index_expression", "resource.id": idx,
                "index_uuid": "_na_"
            });
            let mut error = json!({
                "type": "index_not_found_exception",
                "reason": reason,
                "index": idx, "resource.type": "index_expression", "resource.id": idx,
                "index_uuid": "_na_",
                "root_cause": [cause]
            });
            add_stack_trace(&mut error, &p, "mget");
            docs.push(json!({"_index": idx, "_id": id, "error": error}));
            continue;
        };
        // an alias in front of several indices names no one document, so a
        // get through it cannot say which was meant
        let behind = store.resolve(&idx);
        if store.is_alias(&idx) && behind.len() > 1 {
            let listed = behind.join(", ");
            let reason = format!(
                "alias [{idx}] has more than one index associated with it [{listed}], can't \
                 execute a single index op"
            );
            docs.push(json!({
                "_index": idx, "_id": id, "found": false,
                "error": {
                    "type": "illegal_argument_exception", "reason": reason,
                    "root_cause": [{"type": "illegal_argument_exception", "reason": reason}],
                }
            }));
            continue;
        }
        if let Some(why) = crate::security::item_refusal(
            &store,
            &["indices:data/read/mget[shard]"],
            &crate::security::layer::indices_for_expr(&store, &idx),
        ) {
            docs.push(
                json!({"_index": idx, "_id": id, "error": crate::security::item_error(&why)}),
            );
            continue;
        }
        let g = st.read();
        let routing_ok = match g.routing.get(&id) {
            Some(have) => want_routing.as_deref() == Some(have.as_str()),
            None => true,
        };
        match read_source_as_asked(&g, &id, &p)
            .filter(|_| routing_ok)
            .filter(|_| crate::security::doc_visible(&store, &g, &id))
        {
            Some(mut src) => {
                crate::security::narrow_source(&store, &g.name, &mut src);
                // a doc may carry its own stored_fields; otherwise the request-level
                // one applies. Either way it suppresses _source unless asked for.
                let per_doc_stored = sel.as_ref().and_then(|s| s.get("__stored")).cloned();
                let stored_spec = per_doc_stored.clone().map(|sf| match sf {
                    Value::String(s) => s,
                    Value::Array(a) => {
                        a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")
                    }
                    _ => String::new(),
                });
                let stored_spec = stored_spec.or_else(|| p.get("stored_fields").cloned());

                let explicit_source = sel
                    .clone()
                    .filter(|s| s.get("__stored").is_none())
                    .or_else(|| body.get("_source").cloned())
                    .or_else(|| source_selector_from_params(&p));

                let mut d = json!({
                    "_index": g.name, "_id": id,
                    "_version": g.version_of(&id),
                    "_seq_no": read_seq(&g, &id).unwrap_or(0), "_primary_term": 1, "found": true
                });
                if let Some(r) = g.routing.get(&id) {
                    d["_routing"] = json!(r);
                }
                let mut wants_source = true;
                if let Some(spec) = &stored_spec {
                    let mut sub = Params::new();
                    sub.insert("stored_fields".into(), spec.clone());
                    if let Some(f) = stored_fields(&src, &sub) {
                        d["fields"] = f;
                    }
                    wants_source =
                        spec.split(',').any(|f| f.trim() == "_source") || explicit_source.is_some();
                }
                if wants_source {
                    let filtered = match &explicit_source {
                        Some(s) => apply_source_selector(&src, s),
                        None => src,
                    };
                    if !filtered.is_null() {
                        d["_source"] = filtered;
                    }
                }
                docs.push(d);
            }
            None => docs.push(json!({"_index": g.name, "_id": id, "found": false})),
        }
    }
    respond(&p, json!({"docs": docs}))
}
