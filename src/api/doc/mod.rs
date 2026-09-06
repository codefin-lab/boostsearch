//! Writing, reading and deleting one document, and the bulk of many.

use super::*;

mod validate;
pub use validate::*;

mod bulk;
mod by_query;
pub use bulk::*;
pub use by_query::*;
mod many;
pub use many::*;
mod termvectors;
pub use termvectors::*;
mod update;
pub use update::*;
mod replica;
pub use replica::*;

/// The arrival order recorded for a document, which is what `_seq_no` reports.
pub fn read_seq(st: &IdxState, id: &str) -> Option<u64> {
    if let Some(seq) = st.pending_seq.get(id) {
        return Some(*seq);
    }
    let searcher = st.realtime.searcher();
    let q = TermQuery::new(Term::from_field_text(st.fields.id, id), IndexRecordOption::Basic);
    let hits = searcher.search(&q, &TopDocs::with_limit(1).order_by_score()).ok()?;
    let (_, addr) = hits.first()?;
    // read from the column rather than the stored document: `_seq` is wanted
    // on every hit that asks for it, and a stored copy would be paid for on
    // every write to serve the far rarer read
    let reader = searcher.segment_readers().get(addr.segment_ord as usize)?;
    reader.fast_fields().u64("_seq").ok()?.first(addr.doc_id)
}

pub fn exists_doc(st: &IdxState, id: &str) -> bool {
    st.is_live(id)
}

pub fn write_doc(
    st: &mut IdxState,
    id: &str,
    source: Value,
    op_type: &str,
) -> std::result::Result<(Value, StatusCode), Response> {
    write_doc_raw(st, id, source, op_type, None)
}

/// A write that carries the caller's own version and conditions.
pub fn write_doc_checked(
    st: &mut IdxState,
    id: &str,
    source: Value,
    op_type: &str,
    raw: Option<String>,
    p: &Params,
) -> std::result::Result<(Value, StatusCode), Response> {
    if let Some(r) = seq_check(st, id, p) {
        return Err(r);
    }
    let forced = version_check(st, id, p)?;
    write_doc_versioned(st, id, source, op_type, raw, forced)
}

pub fn write_doc_raw(
    st: &mut IdxState,
    id: &str,
    source: Value,
    op_type: &str,
    raw: Option<String>,
) -> std::result::Result<(Value, StatusCode), Response> {
    write_doc_versioned(st, id, source, op_type, raw, None)
}

pub fn write_doc_versioned(
    st: &mut IdxState,
    id: &str,
    source: Value,
    op_type: &str,
    raw: Option<String>,
    forced: Option<u64>,
) -> std::result::Result<(Value, StatusCode), Response> {
    // an index held still refuses writes until the block is lifted
    if st.knobs.blocks_write {
        return Err(err(
            StatusCode::FORBIDDEN,
            "cluster_block_exception",
            format!("index [{}] blocked by: [FORBIDDEN/8/index write (api)];", st.name),
        ));
    }
    // an id is carried in the index's terms, which caps how long it may be
    if id.len() > 512 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "Document id cannot be longer than 512 bytes but was [{}]. The invalid id was: \
                 [{id}].",
                id.len()
            ),
        ));
    }
    // a create says the document must not be there, which is a version rule
    // of its own; naming another one asks for two things at once
    if op_type == "create" && forced.is_some() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: create operations only support internal versioning. \
             use index instead;",
        ));
    }
    let existed = exists_doc(st, id);
    // the source as it was, for a watched index's record of the change
    let audit_before: Option<Value> = if existed && crate::security::audit_watches_write(&st.name) {
        read_source_as_asked(st, id, &Params::new())
    } else {
        None
    };
    if op_type == "create" && existed {
        return Err(err(
            StatusCode::CONFLICT,
            "version_conflict_engine_exception",
            format!("[{id}]: version conflict, document already exists"),
        ));
    }
    let (version, seq) = match forced {
        Some(v) => st.bump_to(id, true, v),
        None => st.bump(id, true, existed),
    };
    crate::security::audit_document_written(
        &st.name,
        id,
        version,
        audit_before.as_ref(),
        Some(&source),
        false,
    );
    // the shard a write belongs to decides which refresh will show it
    let shard = st.shard_of_doc(id);
    // deleting is only needed when something is actually there to replace;
    // a bulk load of new documents should not queue a delete per document
    if existed {
        st.queue_op(shard, crate::store::PendingOp::Delete(id.to_string()));
    }
    if let Some((kind, reason, cause)) = document_complaint(st, &source) {
        return Err(err_caused_by(&kind, &reason, &cause));
    }
    let default_lenient = st.knobs.ignore_malformed;
    let ignored = match crate::store::scan_malformed(&source, &st.mapping, default_lenient) {
        Ok(v) => v,
        Err((field, ty)) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "mapper_parsing_exception",
                format!("failed to parse field [{field}] of type [{ty}]"),
            ));
        }
    };
    // the ignored names ride along inside the stored source and are lifted back
    // out on the way to a hit, so no second stored field is needed
    let raw = if ignored.is_empty() {
        raw.unwrap_or_else(|| source.to_string())
    } else {
        let mut with = source.clone();
        if let Some(o) = with.as_object_mut() {
            o.insert(
                "_ignored".into(),
                Value::Array(ignored.iter().cloned().map(Value::from).collect()),
            );
        }
        with.to_string()
    };
    st.has_doc_count |= source.get("_doc_count").is_some();
    if let Err(field) = st.mapping.apply_dynamic_templates(&source) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "strict_dynamic_mapping_exception",
            format!(
                "mapping set to strict_allow_templates, dynamic introduction of [{field}] \
                 within [_doc] is not allowed"
            ),
        ));
    }
    // a query stored to be percolated is checked now for what it would fail
    // on later
    if let Some(why) = crate::search::percolator_complaint(st, &source) {
        return Err(err(StatusCode::BAD_REQUEST, "query_shard_exception", why));
    }
    let mut newly_mapped = st.mapping.learn_dynamic(&source);
    // what a derived object holds is learned the way a dynamic field is, so
    // its parts can be searched by type; the object itself stays derived
    if st.mapping.derived_fields().iter().any(|(_, d)| d.get("type") == Some(&json!("object"))) {
        let made: serde_json::Map<String, Value> =
            crate::store::derived_values(&source, &st.mapping)
                .into_iter()
                .filter(|(n, _)| st.mapping.type_of(n) == Some("object"))
                .collect();
        if !made.is_empty() {
            let names: Vec<String> = made.keys().cloned().collect();
            newly_mapped.extend(st.mapping.learn_dynamic(&Value::Object(made)));
            st.mapping.forget_properties(&names);
        }
    }
    if !newly_mapped.is_empty() {
        let body = crate::security::mapping_added_body(&st.mapping.raw, &newly_mapped);
        crate::security::audit_index_event(&st.name, "indices:admin/mapping/auto_put", &body, true);
    }
    // normalized multi-fields are indexed alongside, but never stored
    let mut indexed = crate::store::expand_for_indexing(source, &st.mapping);
    // the kinds a query narrows against have to be the kinds actually indexed,
    // which is the coerced view rather than what the client wrote
    st.observe(&indexed);
    if !ignored.is_empty() {
        for f in &ignored {
            crate::store::remove_path(&mut indexed, f);
        }
        if let Some(o) = indexed.as_object_mut() {
            o.insert(
                "_ignored".into(),
                Value::Array(ignored.iter().cloned().map(Value::from).collect()),
            );
        }
    }
    // a vector goes beside the index rather than into it, and it goes there
    // before the document does: a search that finds the document must be able
    // to find the vector too
    if !st.mapping.vector_fields.is_empty() {
        st.vectors.write().write(&st.mapping.vector_fields, id, &indexed);
    }
    let doc = make_doc(&st.fields, &st.mapping, id, indexed, &raw, seq);
    st.queue_op(shard, crate::store::PendingOp::Add(Box::new(doc)));
    st.bytes.fetch_add(raw.len() as u64, std::sync::atomic::Ordering::Relaxed);
    // recorded before it is answered for: the index has it only after a commit
    let routing = st.routing.get(id).cloned();
    st.log_write(id, routing.as_deref(), version, seq, Some(&raw));
    let term = crate::cluster::primary_term(&st.name, shard as u32);
    // the copies hear of it once the request is answered for here
    crate::cluster::replication::record(crate::cluster::replication::ReplicaOp {
        index: st.name.clone(),
        id: id.to_string(),
        routing,
        version,
        seq,
        term,
        shard: shard as u32,
        source: Some(raw.clone()),
    });
    st.note_pending(id, Some(raw));
    st.note_pending_seq(id, seq);
    let status = if existed { StatusCode::OK } else { StatusCode::CREATED };
    let body = json!({
        "_index": st.name,
        "_id": id,
        "_version": version,
        "result": if existed { "updated" } else { "created" },
        "_shards": shards_of(st),
        "_seq_no": seq,
        "_primary_term": term,
    });
    Ok((body, status))
}

pub fn delete_doc(st: &mut IdxState, id: &str) -> (Value, StatusCode) {
    let existed = exists_doc(st, id);
    let (version, seq) = st.bump(id, false, existed);
    let shard = st.shard_of_doc(id);
    if existed {
        st.queue_op(shard, crate::store::PendingOp::Delete(id.to_string()));
        if !st.mapping.vector_fields.is_empty() {
            st.vectors.write().forget(id);
        }
        st.log_write(id, None, version, seq, None);
        st.note_pending(id, None);
        st.note_pending_seq(id, seq);
        crate::cluster::replication::record(crate::cluster::replication::ReplicaOp {
            index: st.name.clone(),
            id: id.to_string(),
            routing: None,
            version,
            seq,
            term: crate::cluster::primary_term(&st.name, shard as u32),
            shard: shard as u32,
            source: None,
        });
    }
    let body = json!({
        "_index": st.name,
        "_id": id,
        "_version": version,
        "result": if existed { "deleted" } else { "not_found" },
        "_shards": shards_of(st),
        "_seq_no": seq,
        "_primary_term": crate::cluster::primary_term(&st.name, shard as u32),
    });
    (body, if existed { StatusCode::OK } else { StatusCode::NOT_FOUND })
}

/// A write that asked to refresh refreshes the shard it was written to, which
/// is as far as a refresh reaches in OpenSearch. Without a shard to name --
/// a bulk, which may have written to any of them -- everything is refreshed.
/// Replay what the translogs were holding when the process last stopped.
///
/// A write is acknowledged once it is recorded; it reaches the index at the
/// next commit. Anything between those two points is in the translog and
/// nowhere else, so it is replayed here before the server answers anything.
/// Replaying a write that did make it into the index is harmless: an index by
/// id replaces, a delete of what is not there is a no-op.
pub fn recover(store: &Store) {
    for name in store.names() {
        let Some(st) = store.get(&name) else { continue };
        let path = { st.read().path.clone() };
        let Some(dir) = path else { continue };
        let Ok(text) = std::fs::read_to_string(dir.join(crate::store::TRANSLOG)) else { continue };
        let mut replayed = 0usize;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
            let Some(id) = rec.get("id").and_then(|v| v.as_str()) else { continue };
            let version = rec.get("version").and_then(|v| v.as_u64()).unwrap_or(1);
            let mut g = st.write();
            // the write takes back the sequence number it was answered with,
            // and the counter never goes backwards: the numbers a restart
            // hands out have to be past every number already on a document
            if let Some(seq) = rec.get("seq").and_then(|v| v.as_u64()) {
                g.seq_no = g.seq_no.max(seq);
            }
            // a record written before the source went in as a value holds it
            // as a string; both are read here
            let held = match rec.get("source") {
                Some(Value::String(raw)) => serde_json::from_str::<Value>(raw).ok(),
                Some(v @ (Value::Object(_) | Value::Array(_))) => Some(v.clone()),
                _ => None,
            };
            match held {
                Some(source) => {
                    let raw = source.to_string();
                    match rec.get("routing").and_then(|v| v.as_str()) {
                        Some(r) => {
                            g.routing.insert(id.to_string(), r.to_string());
                        }
                        None => {
                            g.routing.remove(id);
                        }
                    }
                    let _ =
                        write_doc_versioned(&mut g, id, source, "index", Some(raw), Some(version));
                }
                _ => {
                    let (_, _) = g.bump_to(id, false, version);
                    let shard = g.shard_of_doc(id);
                    g.queue_op(shard, crate::store::PendingOp::Delete(id.to_string()));
                    g.note_pending(id, None);
                }
            }
            replayed += 1;
        }
        if replayed > 0 {
            // committing puts them in the index, which is what makes the
            // record spent
            let _ = st.write().refresh();
            tracing::warn!("index [{name}]: recovered {replayed} writes from the translog");
        }
    }
}

pub(crate) fn maybe_refresh(st: &mut IdxState, p: &Params, shard: Option<u64>) {
    // the write is about to be answered for, so what was recorded of it has to
    // be on disk -- a refresh commits and makes that moot, but most writes are
    // not refreshed
    st.sync_translog();
    if flag(p, "refresh") {
        let _ = match shard {
            Some(one) => st.refresh_shard(one),
            None => st.refresh(),
        };
    }
}

/// A write that was asked to refresh says so in its answer, so the caller can
/// tell a refresh it forced from one that happened to be due anyway.
pub(crate) fn note_forced_refresh(body: &mut Value, p: &Params) {
    // `wait_for` waits for a refresh that was coming anyway; it does not force
    // one, and so is not reported as having done
    if matches!(p.get("refresh").map(|s| s.as_str()), Some("true") | Some("")) {
        body["forced_refresh"] = json!(true);
    }
}

pub async fn index_doc(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    if !p.contains_key("pipeline")
        && let Some(r) = refuse_unless_alias(&store, &index, &p)
    {
        return r;
    }
    do_index(store, index, Some(id), p, body, "index").await
}

pub async fn index_doc_auto(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    if !p.contains_key("pipeline")
        && let Some(r) = refuse_unless_alias(&store, &index, &p)
    {
        return r;
    }
    do_index(store, index, None, p, body, "index").await
}

pub async fn create_doc(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    do_index(store, index, Some(id), p, body, "create").await
}

pub(crate) async fn do_index(
    store: Store,
    index: String,
    id: Option<String>,
    mut p: Params,
    body: String,
    default_op: &str,
) -> Response {
    let source: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, "mapper_parsing_exception", e.to_string()),
    };
    if !source.is_object() {
        return err(
            StatusCode::BAD_REQUEST,
            "mapper_parsing_exception",
            "failed to parse: expected an object",
        );
    }
    if let Some(bad) = dotted_only_field(&source) {
        return err(
            StatusCode::BAD_REQUEST,
            "mapper_parsing_exception",
            format!("field name cannot contain only the character [.]: [{bad}]"),
        );
    }
    let op_type = p.get("op_type").map(|s| s.as_str()).unwrap_or(default_op).to_string();
    // a pipeline may change the document, its id, its routing, or the index
    // it goes to -- or drop it
    let asked_pipeline = p.get("pipeline").map(|s| s.as_str());
    let mut index = index;
    let mut id = id;
    let mut source = source;
    let mut routing_from_pipeline: Option<Option<String>> = None;
    if asked_pipeline.is_some() || !crate::api::pipelines_for_write(&store, &index, None).is_empty()
    {
        let given_id = id.clone().unwrap_or_default();
        let routing = p.get("routing").filter(|r| !r.is_empty()).cloned();
        let given = crate::api::WriteMeta {
            version: p.get("version").and_then(|v| v.parse().ok()),
            version_type: p.get("version_type").cloned(),
            if_seq_no: p.get("if_seq_no").and_then(|v| v.parse().ok()),
            if_primary_term: p.get("if_primary_term").and_then(|v| v.parse().ok()),
        };
        match crate::api::ingest_for_write_with(
            &store,
            &index,
            &given_id,
            source,
            asked_pipeline,
            routing,
            given,
        ) {
            Ok(Some(d)) => {
                index = d.index.clone();
                if !d.id.is_empty() {
                    id = Some(d.id.clone());
                }
                routing_from_pipeline = Some(d.routing.clone());
                // what the script set on the document's metadata is what the
                // write is asked for
                if let Some(v) = d.version {
                    p.insert("version".into(), v.to_string());
                }
                if let Some(v) = &d.version_type {
                    p.insert("version_type".into(), v.clone());
                }
                if let Some(v) = d.if_seq_no {
                    p.insert("if_seq_no".into(), v.to_string());
                }
                if let Some(v) = d.if_primary_term {
                    p.insert("if_primary_term".into(), v.to_string());
                }
                source = d.source;
                // sent to another index: the request's alias rule applies there,
                // and an alias stands for the index behind it
                if store.is_alias(&index)
                    && let Some(behind) = store.resolve(&index).into_iter().next()
                {
                    index = behind;
                } else if p.get("require_alias").map(|v| v != "false").unwrap_or(false)
                    && !store.is_alias(&index)
                {
                    return err(
                        StatusCode::NOT_FOUND,
                        "index_not_found_exception",
                        format!(
                            "no such index [{index}] and [require_alias] request flag is [true] and \
                             [{index}] is not an alias"
                        ),
                    );
                }
            }
            Ok(None) => {
                return (
                    StatusCode::OK,
                    axum::Json(json!({
                        "_index": index, "_id": id.unwrap_or_default(), "_version": -3,
                        "result": "noop", "_shards": {"total": 0, "successful": 0, "failed": 0},
                        "_seq_no": 0, "_primary_term": 0
                    })),
                )
                    .into_response();
            }
            Err(e) => return crate::api::ingest_failure(&e),
        }
    }
    let was_there = store.get(&index).is_some();
    let st = match store.ensure(&index) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string()),
    };
    if !was_there {
        crate::security::audit_index_event(&index, "indices:admin/auto_create", "{}", false);
    }
    let mut g = st.write();
    let id = id.unwrap_or_else(|| g.next_auto_id());
    // A document written with a routing is only reachable by quoting the same
    // routing back, so it has to be remembered -- before the write, because
    // the routing is also what says which shard the write lands on.
    let routed = match routing_from_pipeline {
        Some(r) => r,
        None => p.get("routing").filter(|r| !r.is_empty()).cloned(),
    };
    match &routed {
        Some(r) => {
            g.routing.insert(id.clone(), r.clone());
        }
        None => {
            g.routing.remove(&id);
        }
    }
    match write_doc_checked(&mut g, &id, source, &op_type, None, &p) {
        Ok((mut body, status)) => {
            if let Some(r) = &routed {
                body["_routing"] = json!(r);
            }
            let shard = g.shard_of_doc(&id);
            maybe_refresh(&mut g, &p, Some(shard));
            note_forced_refresh(&mut body, &p);
            (status, axum::Json(body)).into_response()
        }
        Err(resp) => resp,
    }
}

pub async fn get_doc(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    refresh_before_read(&store, &index, &p);
    let Some(st) = store.get(&index) else {
        return if ignored(&p, StatusCode::NOT_FOUND) {
            (StatusCode::NOT_FOUND, axum::Json(json!({"_index": index, "_id": id, "found": false})))
                .into_response()
        } else {
            no_such_index(&index)
        };
    };
    let g = st.read();
    // a version named on a read is a condition: the document must be at it
    if let Some(want) = p.get("version").and_then(|v| v.parse::<u64>().ok())
        && exists_doc(&g, &id)
        && g.version_of(&id) != want
    {
        return err(
            StatusCode::CONFLICT,
            "version_conflict_engine_exception",
            format!(
                "[{id}]: version conflict, current version [{}] is different than the one \
                     provided [{want}]",
                g.version_of(&id)
            ),
        );
    }
    match read_source_as_asked(&g, &id, &p)
        .filter(|_| routing_matches(&g, &id, &p))
        .filter(|_| crate::security::doc_visible(&store, &g, &id))
    {
        Some(mut src) => {
            crate::security::audit_document_read(&g.name, &id, &src);
            crate::security::narrow_source(&store, &g.name, &mut src);
            let fields = stored_fields(&src, &p);
            let mut body = json!({
                "_index": g.name, "_id": id,
                "_version": g.version_of(&id),
                "_seq_no": read_seq(&g, &id).unwrap_or(0), "_primary_term": 1,
                "found": true,
            });
            if let Some(r) = g.routing.get(&id) {
                body["_routing"] = json!(r);
            }
            // a mapping that keeps the size of each document reports it as
            // the bytes the source takes, when asked for it as a stored field
            let asked_size = p
                .get("stored_fields")
                .map(|s| s.split(',').any(|f| f.trim() == "_size"))
                .unwrap_or(false);
            if asked_size && g.mapping.raw.pointer("/_size/enabled") == Some(&json!(true)) {
                body["_size"] = json!(src.to_string().len());
            }
            if let Some(f) = fields {
                body["fields"] = f;
                // OpenSearch omits _source when only stored_fields were asked for
                if !p.contains_key("_source")
                    && !p.contains_key("_source_includes")
                    && !wants_source_via_stored_fields(&p)
                {
                    return axum::Json(body).into_response();
                }
            }
            body["_source"] = filter_source_params(&src, &p);
            axum::Json(body).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"_index": g.name, "_id": id, "found": false})),
        )
            .into_response(),
    }
}

pub async fn head_doc(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    refresh_before_read(&store, &index, &p);
    let Some(st) = store.get(&index) else { return StatusCode::NOT_FOUND.into_response() };
    let g = st.read();
    // the same view a `_get` would take, so `realtime=false` says whether a
    // search can see the document rather than whether it was written -- and
    // the wrong routing reaches nothing, here as there
    if read_source_as_asked(&g, &id, &p)
        .filter(|_| routing_matches(&g, &id, &p))
        .filter(|_| crate::security::doc_visible(&store, &g, &id))
        .is_some()
    {
        StatusCode::OK.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

pub async fn delete_doc_route(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    let Some(st) = store.get(&index) else { return no_such_index(&index) };
    let mut g = st.write();
    if let Some(r) = seq_check(&g, &id, &p) {
        return r;
    }
    // an external version numbers the delete too, so a stale one is refused
    match version_check(&g, &id, &p) {
        Ok(Some(v)) => {
            let existed = exists_doc(&g, &id);
            let (version, seq) = g.bump_to(&id, false, v);
            // read before the routing is forgotten: it says which shard the
            // document was on, and so which refresh can show the delete
            let shard = g.shard_of_doc(&id);
            if existed {
                g.queue_op(shard, crate::store::PendingOp::Delete(id.to_string()));
                g.log_write(&id, None, version, seq, None);
                g.note_pending(&id, None);
                crate::security::audit_document_written(&g.name, &id, version, None, None, true);
            }
            g.routing.remove(&id);
            maybe_refresh(&mut g, &p, Some(shard));
            let mut body = json!({
                "_index": g.name, "_id": id, "_version": version,
                "result": if existed { "deleted" } else { "not_found" },
                "_shards": shards_of(&g), "_seq_no": seq, "_primary_term": 1,
            });
            note_forced_refresh(&mut body, &p);
            let status = if existed { StatusCode::OK } else { StatusCode::NOT_FOUND };
            return (status, axum::Json(body)).into_response();
        }
        Ok(None) => {}
        Err(r) => return r,
    }
    if !routing_matches(&g, &id, &p) {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({
                "_index": g.name, "_id": id, "_version": g.version_of(&id),
                "result": "not_found", "_shards": shards(),
                "_seq_no": 0, "_primary_term": 1,
            })),
        )
            .into_response();
    }
    let shard = g.shard_of_doc(&id);
    let (mut body, status) = delete_doc(&mut g, &id);
    if body.get("result").and_then(|r| r.as_str()) == Some("deleted") {
        let version = body.get("_version").and_then(|v| v.as_u64()).unwrap_or(0);
        crate::security::audit_document_written(&g.name, &id, version, None, None, true);
    }
    g.routing.remove(&id);
    maybe_refresh(&mut g, &p, Some(shard));
    note_forced_refresh(&mut body, &p);
    (status, axum::Json(body)).into_response()
}

pub async fn count(
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
    // `_count` accepts only a query; anything else is a client error
    if let Some(o) = body.as_object() {
        for k in o.keys() {
            if k != "query" {
                return err(
                    StatusCode::BAD_REQUEST,
                    "parsing_exception",
                    format!("request does not support [{k}]"),
                );
            }
        }
    }
    fold_params_into_body(&mut body, &p);
    body["size"] = json!(0);
    body["track_total_hits"] = json!(true);
    match crate::search::run(&store, &expr, &body, &p) {
        Ok(out) => {
            let n = out.shards;
            let skipped = out.skipped;
            respond(
                &p,
                json!({
                    "count": out.total,
                    "_shards": {"total": n, "successful": n, "skipped": skipped, "failed": 0}
                }),
            )
        }
        Err(r) => r,
    }
}

pub async fn explain(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    // a body that holds something other than a query is not asking about one
    if body.as_object().map(|o| !o.is_empty()).unwrap_or(false) && body.get("query").is_none() {
        let first = body.as_object().and_then(|o| o.keys().next().cloned()).unwrap_or_default();
        return err(
            StatusCode::BAD_REQUEST,
            "parsing_exception",
            format!("request does not support [{first}]"),
        );
    }
    let Some(st) = store.get(&index) else { return no_such_index(&index) };
    let (name, src) = {
        let g = st.read();
        (g.name.clone(), read_source(&g, &id))
    };
    let Some(src) = src else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"_index": name, "_id": id, "matched": false})),
        )
            .into_response();
    };

    // decide matched by running the query restricted to this document
    // `q` names a query string on the URL, with `df` saying which field it
    // reads by default and `default_operator` how its words are joined
    let q = match body.get("query").cloned() {
        Some(q) => q,
        None => match p.get("q").filter(|v| !v.is_empty()) {
            Some(text) => {
                let mut qs = json!({"query": text});
                if let Some(df) = p.get("df") {
                    qs["default_field"] = json!(df);
                }
                if let Some(op) = p.get("default_operator") {
                    qs["default_operator"] = json!(op);
                }
                if p.get("lenient").map(|v| v != "false").unwrap_or(false) {
                    qs["lenient"] = json!(true);
                }
                // the query may be cut with an analyzer the caller names
                if let Some(named) = p.get("analyzer") {
                    qs["analyzer"] = json!(named);
                }
                json!({"query_string": qs})
            }
            None => json!({"match_all": {}}),
        },
    };
    if let Some(st) = store.get(&name) {
        let g = st.read();
        if exists_doc(&g, &id) && !crate::security::doc_visible(&store, &g, &id) {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(json!({"_index": name, "_id": id, "matched": false})),
            )
                .into_response();
        }
    }
    let scoped = json!({"bool": {"must": [q], "filter": [{"ids": {"values": [id]}}]}});
    let probe = json!({"query": scoped, "size": 1});
    let matched = crate::search::run(&store, &name, &probe, &Params::new())
        .map(|o| o.total > 0)
        .unwrap_or(false);

    let told = matched
        .then(|| {
            store.get(&name).and_then(|st| {
                let g = st.read();
                let q = crate::security::with_dls(&store, &g.name, Some(q.clone()))
                    .unwrap_or(q.clone());
                crate::search::explain_document(&g, &q, &id)
            })
        })
        .flatten();
    let mut out = json!({
        "_index": name,
        "_id": id,
        "matched": matched,
        "explanation": told.unwrap_or_else(|| json!({
            "value": if matched { 1.0 } else { 0.0 },
            "description": if matched { "match" } else { "no match" },
            "details": []
        })),
    });
    let sel = body.get("_source").cloned().or_else(|| source_selector_from_params(&p));
    if let Some(sel) = sel.as_ref().filter(|v| **v != json!(false)) {
        out["get"] = json!({
            "_seq_no": 0, "_primary_term": 1, "found": true,
            "_source": apply_source_selector(&src, sel)
        });
    }
    respond(&p, out)
}
