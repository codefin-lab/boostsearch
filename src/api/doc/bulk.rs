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
        // an action that names no document is a request that was cut short:
        // writing an empty document in its place overwrote whatever the
        // action named, and said `"errors": false` about it
        if op != "delete" && doc_line.is_none() {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                "The bulk request must be terminated by a newline [\\n]",
            );
        }
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
                crate::security::item_refusal_audited(&store, &list, &idx, &targets, || None)
                    .map(|why| (idx, why))
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

    // the index routing each name the bulk writes through carries, looked up
    // once per name
    let mut alias_routings: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
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
        // an index a write would create is created with the same two checks a
        // `PUT /{index}` goes through: the name it may have, and whether the
        // cluster lets an index appear this way
        if let Some(refusal) = crate::api::indices::auto_create_refusal(&store, &idx) {
            errors = true;
            let (status, error) = crate::api::error_parts(refusal).await;
            items.push(json!({ op.clone(): {
                "_index": idx, "_id": id_opt.clone().unwrap_or_default(),
                "status": status, "error": error,
            }}));
            continue;
        }
        crate::api::datastream::create_stream_for_write(&store, &idx);
        let was_there = store.get(&idx).is_some();
        // an alias writes to the index it marks as the write index, and to
        // nowhere else: `ensure` answers with whichever backing index the map
        // iterated to first, so after a rollover a write could land back in
        // the index that had just been rolled out of
        // the alias with no write index was refused above, so what is left
        // here is a name that has one, or a name that is not an alias
        let named = idx.clone();
        let idx = store.write_target(&idx).unwrap_or(idx);
        let st = match store.ensure(&idx) {
            Ok(s) => s,
            Err(e) => {
                return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string());
            }
        };
        if !was_there {
            crate::security::audit_index_event(&idx, "indices:admin/auto_create", "{}", false);
        }
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
            // where to run a pipeline is decided for the request, not for one
            // line of it: a cluster with no ingest node has nowhere to send
            // any of them, so the whole bulk is refused rather than each line
            // failing on its own
            if !crate::api::ingest::any_ingest_node() {
                return err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    "There are no ingest nodes in this cluster, unable to forward request to \
                     an ingest node.",
                );
            }
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
                            Err(e) => {
                                errors = true;
                                failed_item(&op, &d.index, &new_id, e)
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
        // the routing this item carries: its own, or the one the alias it was
        // written through supplies -- read before this index is locked, since
        // looking an alias up reads every index
        let asked_routing = meta.get("routing").and_then(scalar_str);
        let item_routing = match pipeline_routing.clone() {
            Some(r) => Ok(r),
            None => {
                let alias_routing = alias_routings
                    .entry(named.clone())
                    .or_insert_with(|| store.alias_index_routing(&named))
                    .clone();
                routing_through(&named, alias_routing, asked_routing)
            }
        };
        let item_routing = match item_routing {
            Ok(r) => r,
            Err(refusal) => {
                errors = true;
                items.push(failed_item(&op, &idx, id_opt.as_deref().unwrap_or(""), refusal));
                continue;
            }
        };
        let mut g = st.write();
        let id_was_given = id_was_given_before;
        let id = id_opt.unwrap_or_else(|| g.next_auto_id());
        let refusal = match op.as_str() {
            "index" | "create" => routing_refusal(&g, &id, item_routing.as_deref()),
            _ => {
                let mut asked = Params::new();
                if let Some(r) = &item_routing {
                    asked.insert("routing".into(), r.clone());
                }
                read_routing_refusal(&g, &id, &asked)
            }
        };
        if let Some(refusal) = refusal {
            errors = true;
            let status = refusal.status().as_u16();
            let error = match refusal.extensions().get::<crate::api::shared::ErrorKind>() {
                Some(k) if k.kind == "routing_missing_exception" => {
                    crate::api::shared::routing_missing_cause(&g.name, &id)
                }
                Some(k) => json!({"type": k.kind, "reason": k.reason}),
                None => json!({"type": "exception", "reason": "refused"}),
            };
            items.push(json!({ op.clone(): {
                "_index": g.name, "_id": id, "status": status, "error": error,
            }}));
            continue;
        }
        let mut routed_params = Params::new();
        if let Some(r) = &item_routing {
            routed_params.insert("routing".into(), r.clone());
        }

        let item = match op.as_str() {
            "delete" if !routing_matches(&g, &id, &routed_params) => {
                // the shard the routing names does not hold the document
                json!({ "delete": {
                    "_index": g.name, "_id": id, "_version": 1, "result": "not_found",
                    "_shards": shards_of(&g), "_seq_no": 0, "_primary_term": 1, "status": 404,
                }})
            }
            "delete" => {
                let (body, status) = delete_doc(&mut g, &id);
                // a delete the index refused is an error in this bulk, and
                // the flag at the top of the answer is what every client
                // reads to decide whether the request went through
                if !status.is_success() {
                    errors = true;
                }
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
                // number the caller believes the document is at. A document
                // that is not there is at no sequence number at all, which is
                // a conflict with any the caller could name.
                let cond = meta.get("if_seq_no").and_then(|v| v.as_u64());
                if let Some(want) = cond {
                    let here = exists_doc(&g, &id);
                    let have = if here { read_seq(&g, &id).unwrap_or(0) as i64 } else { -2 };
                    if have != want as i64 {
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
                match item_routing.clone() {
                    Some(r) => {
                        g.routing.insert(id.clone(), r);
                    }
                    None => {
                        g.routing.remove(&id);
                    }
                }
                // a document a data stream cannot take: no single timestamp
                if matches!(op.as_str(), "index" | "create")
                    && let Some(refusal) =
                        crate::api::datastream::stream_document_refusal(&store, &idx, &src)
                {
                    errors = true;
                    items.push(failed_item(&op, &idx, &id, refusal));
                    continue;
                }
                // a document the mapping cannot accept is one item's failure,
                // not the whole request's
                if let Some((kind, reason, cause)) = document_complaint(&g, &src) {
                    errors = true;
                    let reason = reason.replace("{id}", &id);
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
                    Err(e) => {
                        errors = true;
                        failed_item(&op, &idx, &id, e)
                    }
                }
            }
            "update" => {
                let existing =
                    read_source(&g, &id).filter(|_| routing_matches(&g, &id, &routed_params));
                // a routing named on the item is the one the document is
                // written with
                // written with -- unless it names another shard than the one
                // holding the document, which it then leaves alone
                if let Some(r) = &item_routing
                    && (existing.is_some() || !exists_doc(&g, &id))
                {
                    g.routing.insert(id.clone(), r.clone());
                }
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
                    // a script over the document that is there: the same
                    // work the single-document update does, reported as an
                    // item rather than as the whole request
                    (Some(base), None) if patch.get("script").is_some() => {
                        let spec = patch.get("script").cloned().unwrap_or(Value::Null);
                        let compiled = match crate::painless::contexts::Compiled::of(&spec, &|n| {
                            store.stored_script(n)
                        }) {
                            Ok(c) => c,
                            Err(e) => {
                                errors = true;
                                items.push(json!({"update": {
                                    "_index": idx, "_id": id, "status": 400, "error": e.to_json()
                                }}));
                                continue;
                            }
                        };
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(0);
                        let ctx = crate::painless::contexts::update_ctx(
                            &idx,
                            &id,
                            g.version_of(&id),
                            &base,
                            now,
                            "index",
                        );
                        let mut runner = crate::painless::contexts::Runner::new(&compiled.params)
                            .with_ctx(ctx.clone());
                        if let Err(e) = runner.run(&compiled.script) {
                            errors = true;
                            items.push(json!({"update": {
                                "_index": idx, "_id": id, "status": 400, "error": e.to_json()
                            }}));
                            continue;
                        }
                        let Ok((op, source, _, _)) = crate::painless::contexts::read_ctx(&ctx)
                        else {
                            errors = true;
                            items.push(json!({"update": {
                                "_index": idx, "_id": id, "status": 400
                            }}));
                            continue;
                        };
                        match op.as_str() {
                            "noop" | "none" => json!({"update": {
                                "_index": idx, "_id": id, "_version": g.version_of(&id),
                                "result": "noop", "status": 200
                            }}),
                            "delete" => {
                                let (body, status) = delete_doc(&mut g, &id);
                                let mut b = body;
                                // a refusal is this item's failure, not a
                                // delete: writing "deleted" over it told a
                                // retention job the document was gone while
                                // it was still there
                                if status.is_success() {
                                    b["result"] = json!("deleted");
                                }
                                b["status"] = json!(status.as_u16());
                                if !status.is_success() {
                                    errors = true;
                                }
                                json!({"update": b})
                            }
                            _ => match write_doc(&mut g, &id, source, "index") {
                                Ok((body, _)) => {
                                    let mut b = body;
                                    b["result"] = json!("updated");
                                    b["status"] = json!(200);
                                    json!({"update": b})
                                }
                                Err(_) => {
                                    errors = true;
                                    json!({"update": {"_index": idx, "_id": id, "status": 500}})
                                }
                            },
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
    // an index whose record did not reach the disk answers for none of the
    // items written to it in this bulk
    let mut unrecorded: Vec<(String, String)> = Vec::new();
    for n in touched {
        let Some(st) = store.get(&n) else { continue };
        let mut g = st.write();
        if refreshing {
            let _ = g.refresh();
        }
        if let Err(why) = g.sync_translog() {
            unrecorded.push((n.clone(), why));
        }
    }
    for item in items.iter_mut() {
        let Some(o) = item.as_object_mut().and_then(|o| o.values_mut().next()) else {
            continue;
        };
        let index = o.get("_index").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if o.get("error").is_none()
            && let Some((_, why)) = unrecorded.iter().find(|(n, _)| *n == index)
        {
            errors = true;
            o["status"] = json!(500);
            o["error"] = json!({"type": "translog_exception", "reason": why});
            if let Some(m) = o.as_object_mut() {
                m.remove("result");
            }
        }
    }
    // a bulk asked to refresh says so of every item that wrote something, as
    // a single write does; a noop and a failure wrote nothing to show
    if matches!(p.get("refresh").map(|s| s.as_str()), Some("true") | Some("")) {
        for item in items.iter_mut() {
            if let Some(o) = item.as_object_mut().and_then(|o| o.values_mut().next())
                && o.get("error").is_none()
                && o.get("result").and_then(|r| r.as_str()).is_some_and(|r| r != "noop")
            {
                o["forced_refresh"] = json!(true);
            }
        }
    }
    let mut out = json!({
        // a write that took less than a millisecond still took some time:
        // OpenSearch's own clock never reports a bulk as instantaneous, and a
        // client that measures throughput divides by this
        "took": (started.elapsed().as_millis() as u64).max(1),
        "errors": errors,
        "items": items,
    });
    if ingested {
        out["ingest_took"] = json!(0);
    }
    axum::Json(out).into_response()
}

/// What one operation's failure says, as an item in the answer.
///
/// A write may fail for a reason of its own -- a document the mapping cannot
/// parse, an index held still -- and that reason is the item's, not a made-up
/// conflict; the error travels beside the response it was written into.
fn failed_item(op: &str, index: &str, id: &str, e: Response) -> Value {
    match e.extensions().get::<crate::api::shared::ErrorKind>() {
        // an item's error is the cause itself, with where it happened, as
        // the reference writes it: no `root_cause` list inside an item
        Some(k) => {
            let mut error = json!({"type": k.kind, "reason": k.reason});
            if let Some(w) = e.extensions().get::<crate::api::shared::DocWhere>() {
                error["index"] = json!(w.index);
                error["shard"] = json!(w.shard.to_string());
                error["index_uuid"] = json!(w.uuid);
            }
            if let Some(crate::api::shared::ErrorCause(cause)) = e.extensions().get() {
                error["caused_by"] = cause.clone();
            }
            json!({ op: {
                "_index": index, "_id": id, "status": e.status().as_u16(), "error": error
            }})
        }
        None => json!({ op: {
            "_index": index, "_id": id, "status": 409,
            "error": {"type": "version_conflict_engine_exception",
                      "reason": format!("[{id}]: version conflict, document already exists")}
        }}),
    }
}
