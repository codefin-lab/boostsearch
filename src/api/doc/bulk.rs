//! Many writes in one request.

use super::*;

pub async fn bulk(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    _headers: HeaderMap,
    body: String,
) -> Response {
    let started = std::time::Instant::now();
    if let Some(b) = p.get("batch_size")
        && b.parse::<i64>().map(|n| n < 1).unwrap_or(true)
    {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("Batch size must be greater than 0, but got [{b}]"),
        );
    }
    let default_index = index.map(|Path(i)| i);
    let mut items = Vec::new();
    let mut errors = false;
    // `ingest_took` is reported only when a pipeline ran on the way in
    let mut ingested = false;
    let mut touched: Vec<String> = Vec::new();

    // Split the ndjson into operations first, so the expensive part -- parsing
    // each document and building its BoostCore form -- can run across cores.
    struct Op<'a> {
        op: String,
        meta: Value,
        index: String,
        id: Option<String>,
        doc_line: Option<&'a str>,
    }
    let mut ops: Vec<Op> = Vec::new();
    // the line a complaint names is counted over the whole body, blank lines
    // and document lines included
    let mut lineno = 0usize;
    let mut lines = body.lines().filter(|l| !l.trim().is_empty()).inspect(|_| {});
    let mut lines = std::iter::from_fn(move || {
        let next = lines.next();
        if next.is_some() {
            lineno += 1;
        }
        next.map(|l| (lineno, l))
    })
    .peekable();
    while let Some((at, action_line)) = lines.next() {
        let action: Value = match serde_json::from_str(action_line) {
            Ok(v) => v,
            Err(e) => {
                return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string());
            }
        };
        // an action names the operation; an object with nothing in it names
        // none, and the line it was on is what a caller needs to be told
        let Some((op, meta)) = action.as_object().and_then(|o| o.iter().next()) else {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!(
                    "Malformed action/metadata line [{at}], expected FIELD_NAME but found \
                     [END_OBJECT]"
                ),
            );
        };
        let op = op.clone();
        let idx = meta.get("_index").and_then(scalar_str).or_else(|| default_index.clone());
        let Some(idx) = idx else {
            return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", "missing index");
        };
        let id_opt = meta.get("_id").and_then(scalar_str);
        let doc_line = if op == "delete" { None } else { lines.next().map(|(_, l)| l) };
        ops.push(Op { op, meta: meta.clone(), index: idx, id: id_opt, doc_line });
    }

    // Parse and build documents in parallel; nothing here touches shared state.
    // the plugin judges each index's share of the bulk as one shard request:
    // every action it carries, or none of it
    let refused: std::collections::HashMap<String, String> = {
        let mut wanted: Vec<(String, Vec<&str>)> = Vec::new();
        for o in &ops {
            let action = match o.op.as_str() {
                "delete" => "indices:data/write/delete",
                "update" => "indices:data/write/update",
                _ => "indices:data/write/index",
            };
            match wanted.iter_mut().find(|(i, _)| *i == o.index) {
                Some((_, list)) => {
                    if !list.contains(&action) {
                        list.push(action);
                    }
                }
                None => wanted.push((o.index.clone(), vec!["indices:data/write/bulk[s]", action])),
            }
        }
        wanted
            .into_iter()
            .filter_map(|(idx, list)| {
                let targets = crate::security::layer::indices_for_expr(&store, &idx);
                crate::security::item_refusal(&store, &list, &targets).map(|why| (idx, why))
            })
            .collect()
    };
    let prepare = |o: &Op| {
        o.doc_line.map(|l| {
            serde_json::from_str::<Value>(l)
                .map(|v| (v, l.trim().to_string()))
                .map_err(|e| e.to_string())
        })
    };
    let prepared: Vec<Option<std::result::Result<(Value, String), String>>> =
        if std::env::var("BOOSTSEARCH_SERIAL_BULK").is_ok() {
            ops.iter().map(prepare).collect()
        } else {
            use rayon::prelude::*;
            ops.par_iter().map(prepare).collect()
        };

    // consume the prepared documents rather than cloning them back out
    for (o, prep) in ops.into_iter().zip(prepared) {
        // an index action may carry `op_type: create` in its metadata, which
        // makes it a create -- in what it refuses, and in what it is called
        // in the answer
        let op = match o.meta.get("op_type").and_then(|v| v.as_str()) {
            Some("create") => "create".to_string(),
            _ => o.op,
        };
        let meta = o.meta;
        let idx = o.index;
        let id_opt = o.id;
        if let Some(why) = refused.get(&idx) {
            errors = true;
            items.push(json!({ op.clone(): {
                "_index": idx, "_id": id_opt, "status": 403,
                "error": {"type": "security_exception", "reason": why},
            }}));
            continue;
        }
        let meta_source = meta.get("_source").cloned();
        let (source, mut doc_raw): (Option<Value>, Option<String>) = match prep {
            Some(Ok((v, raw))) => (Some(v), Some(raw)),
            Some(Err(e)) => {
                return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e);
            }
            None => (None, None),
        };

        // an id written as an empty string is not the same as no id at all:
        // one asks for a document that has none, the other for a fresh one
        if id_opt.as_deref() == Some("") {
            errors = true;
            items.push(json!({ op.clone(): {
                "_index": idx, "_id": "", "status": 400,
                "error": {
                    "type": "illegal_argument_exception",
                    "reason": "if _id is specified it must not be empty"
                }
            }}));
            continue;
        }
        // an alias standing in front of several indices has no one place to
        // write to unless one of them was named the write index
        if store.is_alias(&idx) {
            let behind = store.resolve(&idx);
            let has_write = behind.iter().any(|n| {
                store
                    .get(n)
                    .map(|st| {
                        st.read()
                            .aliases
                            .get(&idx)
                            .and_then(|d| d.get("is_write_index"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
            });
            if behind.len() > 1 && !has_write {
                errors = true;
                items.push(json!({ op.clone(): {
                    "_index": idx, "_id": id_opt.clone().unwrap_or_default(), "status": 400,
                    "error": {
                        "type": "illegal_argument_exception",
                        "reason": format!(
                            "no write index is defined for alias [{idx}]. The write index may \
                             be explicitly disabled using is_write_index=false or the alias \
                             points to multiple indices without one being designated as a \
                             write index"
                        )
                    }
                }}));
                continue;
            }
        }
        // `require_alias` says the write is meant for an alias, so a name that
        // is not one is treated as absent rather than created on the spot
        // the action's own flag answers for it; the request's applies to the
        // actions that did not say
        let needs_alias = match meta.get("require_alias").and_then(|v| v.as_bool()) {
            Some(own) => own,
            None => p.get("require_alias").map(|v| v != "false").unwrap_or(false),
        };
        if needs_alias && !store.is_alias(&idx) {
            errors = true;
            items.push(json!({ op.clone(): {
                "_index": idx, "_id": id_opt.clone().unwrap_or_default(), "status": 404,
                "error": {
                    "type": "index_not_found_exception",
                    "reason": format!(
                        "no such index [{idx}] and [require_alias] request flag is [true] and \
                         [{idx}] is not an alias"
                    )
                }
            }}));
            continue;
        }
        let st = match store.ensure(&idx) {
            Ok(s) => s,
            Err(e) => {
                return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string());
            }
        };
        if !touched.contains(&idx) {
            touched.push(idx.clone());
        }
        let id_was_given_before = id_opt.is_some();
        let mut id_opt = id_opt;
        let mut source = source;
        let mut pipeline_routing: Option<Option<String>> = None;
        // keep the number of live writers bounded across indices
        if !g_has_writer(&st) {
            store.note_writer_opened(&idx);
        }
        // the pipelines the action or the index asks for run first;
        // they may change the document or drop it
        let asked_pipeline = meta
            .get("pipeline")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| p.get("pipeline").cloned());
        if matches!(op.as_str(), "index" | "create")
            && (asked_pipeline.is_some()
                || !crate::api::pipelines_for_write(&store, &idx, None).is_empty())
        {
            let src_now = source.take().unwrap_or_else(|| json!({}));
            let routing = meta.get("routing").and_then(|v| v.as_str()).map(|s| s.to_string());
            // the store is asked while this index's lock is held: the
            // pipelines live beside it, not under it
            ingested = true;
            match crate::api::ingest_for_write(
                &store,
                &idx,
                id_opt.as_deref().unwrap_or(""),
                src_now,
                asked_pipeline.as_deref(),
                routing,
            ) {
                Ok(Some(d)) => {
                    if d.index != idx {
                        // sent elsewhere: written there instead
                        let target = match store.ensure(&d.index) {
                            Ok(s) => s,
                            Err(e) => {
                                return err(
                                    StatusCode::BAD_REQUEST,
                                    "illegal_argument_exception",
                                    e.to_string(),
                                );
                            }
                        };
                        if !touched.contains(&d.index) {
                            touched.push(d.index.clone());
                        }
                        let mut tg = target.write();
                        let new_id = if d.id.is_empty() { tg.next_auto_id() } else { d.id.clone() };
                        if let Some(r) = &d.routing {
                            tg.routing.insert(new_id.clone(), r.clone());
                        }
                        let item = match write_doc_raw(&mut tg, &new_id, d.source, &op, None) {
                            Ok((body, status)) => {
                                let mut b = body;
                                b["status"] = json!(status.as_u16());
                                json!({ op.clone(): b })
                            }
                            Err(_) => {
                                errors = true;
                                json!({ op.clone(): {"_index": d.index, "_id": new_id, "status": 409,
                                    "error": {"type": "version_conflict_engine_exception",
                                              "reason": format!("[{new_id}]: version conflict, document already exists")}}})
                            }
                        };
                        items.push(item);
                        continue;
                    }
                    if !d.id.is_empty() {
                        id_opt = Some(d.id.clone());
                    }
                    pipeline_routing = Some(d.routing.clone());
                    source = Some(d.source);
                    // the line as sent is not the document any more
                    doc_raw = None;
                }
                Ok(None) => {
                    items.push(json!({ op.clone(): {
                        "_index": idx, "_id": id_opt.clone().unwrap_or_default(), "_version": -3, "result": "noop",
                        "_shards": {"total": 0, "successful": 0, "failed": 0},
                        "_seq_no": 0, "_primary_term": 0, "status": 200
                    }}));
                    continue;
                }
                Err(e) => {
                    errors = true;
                    items.push(json!({ op.clone(): {
                        "_index": idx, "_id": id_opt.clone().unwrap_or_default(), "status": e.status(),
                        "error": e.body()
                    }}));
                    continue;
                }
            }
        }
        let mut g = st.write();
        let id_was_given = id_was_given_before;
        let id = id_opt.unwrap_or_else(|| g.next_auto_id());

        let item = match op.as_str() {
            "delete" => {
                let (body, status) = delete_doc(&mut g, &id);
                let mut b = body;
                b["status"] = json!(status.as_u16());
                json!({ "delete": b })
            }
            "index" | "create" => {
                if id_was_given && append_only(&g) {
                    errors = true;
                    items.push(json!({ op.clone(): {
                        "_index": idx, "_id": id, "status": 400,
                        "error": {
                            "type": "validation_exception",
                            "reason": format!(
                                "Validation Failed: 1: Operation [{}] is not allowed with a \
                                 custom document id {id} as setting \
                                 `index.append_only.enabled` is enabled for this index: {idx};",
                                op.to_uppercase()
                            )
                        }
                    }}));
                    continue;
                }
                // an index action may be conditional too, on the sequence
                // number the caller believes the document is at
                let cond =
                    meta.get("if_seq_no").and_then(|v| v.as_u64()).filter(|_| exists_doc(&g, &id));
                if let Some(want) = cond {
                    let have = read_seq(&g, &id).unwrap_or(0);
                    if have != want {
                        errors = true;
                        items.push(json!({ op.clone(): {
                            "_index": idx, "_id": id, "status": 409,
                            "error": {
                                "type": "version_conflict_engine_exception",
                                "reason": format!(
                                    "[{id}]: version conflict, required seqNo [{want}], \
                                     primary term [1]. current document has seqNo [{have}] \
                                     and primary term [1]"
                                )
                            }
                        }}));
                        continue;
                    }
                }
                let src = source.unwrap_or_else(|| json!({}));
                // a routing named on the action line places the document, and
                // has to be remembered the same way a single write's does
                let routing_now: Option<String> = match pipeline_routing {
                    Some(r) => r,
                    None => meta
                        .get("routing")
                        .and_then(|v| v.as_str())
                        .filter(|r| !r.is_empty())
                        .map(|s| s.to_string()),
                };
                match routing_now {
                    Some(r) => {
                        g.routing.insert(id.clone(), r);
                    }
                    None => {
                        g.routing.remove(&id);
                    }
                }
                // a document the mapping cannot accept is one item's failure,
                // not the whole request's
                if let Some((kind, reason, cause)) = document_complaint(&g, &src) {
                    errors = true;
                    let mut error = json!({"type": kind, "reason": reason});
                    if !cause.is_empty() {
                        error["caused_by"] =
                            json!({"type": "illegal_argument_exception", "reason": cause});
                    }
                    items.push(json!({ op.clone(): {
                        "_index": idx, "_id": id, "status": 400, "error": error,
                    }}));
                    continue;
                }
                match write_doc_raw(&mut g, &id, src, &op, doc_raw.take()) {
                    Ok((body, status)) => {
                        let mut b = body;
                        b["status"] = json!(status.as_u16());
                        json!({ op.clone(): b })
                    }
                    Err(_) => {
                        errors = true;
                        json!({ op.clone(): {
                            "_index": idx, "_id": id, "status": 409,
                            "error": {
                                "type": "version_conflict_engine_exception",
                                "reason": format!("[{id}]: version conflict, document already exists")
                            }
                        }})
                    }
                }
            }
            "update" => {
                let existing = read_source(&g, &id);
                // the same conditional write the single-document update takes,
                // reported per item rather than as the whole request failing
                let stale =
                    match (meta.get("if_seq_no").and_then(|v| v.as_u64()), existing.is_some()) {
                        (Some(want), true) => Some((want, read_seq(&g, &id).unwrap_or(0)))
                            .filter(|(want, have)| want != have),
                        _ => None,
                    };
                if let Some((want, have)) = stale {
                    errors = true;
                    items.push(json!({ "update": {
                        "_index": idx, "_id": id, "status": 409,
                        "error": {
                            "type": "version_conflict_engine_exception",
                            "reason": format!(
                                "[{id}]: version conflict, required seqNo [{want}], \
                                 primary term [1]. current document has seqNo [{have}] \
                                 and primary term [1]"
                            )
                        }
                    }}));
                    continue;
                }
                let patch = source.unwrap_or_else(|| json!({}));
                let doc = patch.get("doc").cloned();
                match (existing, doc) {
                    (Some(mut base), Some(d)) => {
                        let before = base.clone();
                        merge_into(&mut base, &d);
                        // an update that changes nothing is reported as such,
                        // and counted, the same way the single-document API
                        // reports it
                        let noop =
                            patch.get("detect_noop").and_then(|v| v.as_bool()).unwrap_or(true)
                                && base == before;
                        if noop {
                            g.noop_updates.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        match write_doc(&mut g, &id, base.clone(), "index") {
                            Ok((body, _)) => {
                                let mut b = body;
                                b["result"] = json!(if noop { "noop" } else { "updated" });
                                b["status"] = json!(200);
                                let sel = meta_source
                                    .clone()
                                    .or_else(|| patch.get("_source").cloned())
                                    .or_else(|| source_selector_from_params(&p));
                                if let Some(sel) = sel.as_ref().filter(|v| **v != json!(false)) {
                                    b["get"] = json!({
                                        "_source": apply_source_selector(&base, sel)
                                    });
                                }
                                json!({ "update": b })
                            }
                            Err(_) => {
                                errors = true;
                                json!({"update": {"_index": idx, "_id": id, "status": 500}})
                            }
                        }
                    }
                    (None, _) => {
                        let as_upsert =
                            patch.get("doc_as_upsert").and_then(|v| v.as_bool()).unwrap_or(false);
                        let upsert_doc = patch
                            .get("upsert")
                            .or_else(|| if as_upsert { patch.get("doc") } else { None });
                        if let Some(ups) = upsert_doc {
                            let mut ups = ups.clone();
                            // an upsert makes a document, which goes in through
                            // the index's pipelines like any fresh write
                            let names = crate::api::pipelines_for_state_in(
                                &store,
                                &g,
                                meta.get("pipeline").and_then(|v| v.as_str()),
                            );
                            if !names.is_empty() {
                                let doc = crate::ingest::IngestDoc::new(&idx, &id, ups.clone());
                                match crate::api::run_named_pipelines(&store, names, doc) {
                                    Ok(Some(d)) => ups = d.source,
                                    Ok(None) => {
                                        items.push(json!({"update": {"_index": idx, "_id": id, "_version": -3, "result": "noop", "status": 200}}));
                                        continue;
                                    }
                                    Err(e) => {
                                        errors = true;
                                        items.push(json!({"update": {"_index": idx, "_id": id, "status": e.status(), "error": e.body()}}));
                                        continue;
                                    }
                                }
                            }
                            // a scripted upsert runs the script over the upsert
                            // document before it is written
                            if patch
                                .get("scripted_upsert")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false)
                                && let Some(spec) = patch.get("script")
                            {
                                match crate::painless::contexts::Compiled::of(spec, &|n| {
                                    store.stored_script(n)
                                }) {
                                    Ok(compiled) => {
                                        let now = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .map(|d| d.as_millis() as i64)
                                            .unwrap_or(0);
                                        let ctx = crate::painless::contexts::update_ctx(
                                            &idx, &id, 1, &ups, now, "create",
                                        );
                                        let mut runner = crate::painless::contexts::Runner::new(
                                            &compiled.params,
                                        )
                                        .with_ctx(ctx.clone());
                                        if let Err(e) = runner.run(&compiled.script) {
                                            errors = true;
                                            items.push(json!({"update": {"_index": idx, "_id": id, "status": 400, "error": e.to_json()}}));
                                            continue;
                                        }
                                        if let Ok((_, src, _, _)) =
                                            crate::painless::contexts::read_ctx(&ctx)
                                        {
                                            ups = src;
                                        }
                                    }
                                    Err(e) => {
                                        errors = true;
                                        items.push(json!({"update": {"_index": idx, "_id": id, "status": 400, "error": e.to_json()}}));
                                        continue;
                                    }
                                }
                            }
                            match write_doc(&mut g, &id, ups.clone(), "index") {
                                Ok((body, _)) => {
                                    let mut b = body;
                                    b["status"] = json!(201);
                                    json!({ "update": b })
                                }
                                Err(_) => {
                                    errors = true;
                                    json!({"update": {"_index": idx, "_id": id, "status": 500}})
                                }
                            }
                        } else {
                            errors = true;
                            let reason = format!("[{id}]: document missing");
                            let mut error = json!({
                                "type": "document_missing_exception", "reason": reason,
                            });
                            // this one names the shard it looked in, which is
                            // what a caller reading the trace wants to know
                            if p.get("error_trace").map(|v| v != "false").unwrap_or(false) {
                                error["stack_trace"] = json!(format!(
                                    "[[{idx}][0]] DocumentMissingException[{reason}] \
                                     at boostsearch::api::bulk (src/api.rs)"
                                ));
                            }
                            json!({"update": {
                                "_index": idx, "_id": id, "status": 404, "error": error
                            }})
                        }
                    }
                    _ => {
                        errors = true;
                        json!({"update": {"_index": idx, "_id": id, "status": 400}})
                    }
                }
            }
            other => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!("unknown bulk action [{other}]"),
                );
            }
        };
        items.push(item);
    }

    // one bulk is one write to answer for, so its record is forced once, not
    // once per item; a refresh commits the lot and makes the record moot
    let refreshing = flag(&p, "refresh");
    for n in touched {
        let Some(st) = store.get(&n) else { continue };
        let mut g = st.write();
        if refreshing {
            let _ = g.refresh();
        } else {
            g.sync_translog();
        }
    }
    let mut out = json!({
        "took": started.elapsed().as_millis() as u64,
        "errors": errors,
        "items": items,
    });
    if ingested {
        out["ingest_took"] = json!(0);
    }
    axum::Json(out).into_response()
}
