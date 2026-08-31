//! Writing, reading and deleting one document, and the bulk of many.

use super::*;

mod bulk;
pub use bulk::*;
mod many;
pub use many::*;
mod termvectors;
pub use termvectors::*;
mod update;
pub use update::*;

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

/// Write one document. `op_type == "create"` refuses to overwrite.
pub fn append_only(st: &IdxState) -> bool {
    st.setting("append_only.enabled").map(|v| v == "true").unwrap_or(false)
}

/// A version the caller supplied, and what it means for the write.
///
/// `external` numbers a document from somewhere outside the index: it must
/// climb, so a write carrying a version the index already has, or an older
/// one, has arrived out of order and is refused. `external_gte` allows the
/// same number again. Without a type the number names the version the caller
/// believes is current, and must match.
pub(crate) fn version_check(
    st: &IdxState,
    id: &str,
    p: &Params,
) -> std::result::Result<Option<u64>, Response> {
    let Some(want) = p.get("version").and_then(|v| v.parse::<u64>().ok()) else {
        return Ok(None);
    };
    let ty = p.get("version_type").map(|v| v.as_str()).unwrap_or("internal");
    let existed = exists_doc(st, id);
    let have = st.version_of(id);
    let conflict = |have: u64| {
        Err(err(
            StatusCode::CONFLICT,
            "version_conflict_engine_exception",
            format!(
                "[{id}]: version conflict, current version [{have}] is higher or equal to \
                 the one provided [{want}]"
            ),
        ))
    };
    match ty {
        "external" | "external_gt" => {
            if existed && want <= have {
                return conflict(have);
            }
            Ok(Some(want))
        }
        "external_gte" => {
            if existed && want < have {
                return conflict(have);
            }
            Ok(Some(want))
        }
        _ => {
            if !existed || want != have {
                return Err(err(
                    StatusCode::CONFLICT,
                    "version_conflict_engine_exception",
                    format!(
                        "[{id}]: version conflict, required version [{want}] is different \
                         to the one in the index [{have}]"
                    ),
                ));
            }
            Ok(None)
        }
    }
}

/// `if_seq_no` makes a write conditional on the document not having moved.
pub(crate) fn seq_check(st: &IdxState, id: &str, p: &Params) -> Option<Response> {
    let want_seq = p.get("if_seq_no").and_then(|v| v.parse::<u64>().ok());
    let want_term = p.get("if_primary_term").and_then(|v| v.parse::<u64>().ok());
    if want_seq.is_none() && want_term.is_none() {
        return None;
    }
    if !exists_doc(st, id) {
        return None;
    }
    let have = read_seq(st, id).unwrap_or(0);
    // a shard that has never failed over is on its first term, so any other
    // term the caller insists on is a term this document was not written in
    let term_ok = want_term.map(|t| t == 1).unwrap_or(true);
    let want = want_seq.unwrap_or(have);
    if have == want && term_ok {
        return None;
    }
    let want_term = want_term.unwrap_or(1);
    Some(err(
        StatusCode::CONFLICT,
        "version_conflict_engine_exception",
        format!(
            "[{id}]: version conflict, required seqNo [{want}], primary term \
             [{want_term}]. current document has seqNo [{have}] and primary term [1]"
        ),
    ))
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

/// `raw` is the document exactly as the client sent it; passing it through
/// avoids re-serialising a tree we only just parsed.
/// What is wrong with this document, as a kind, a reason and its cause.
///
/// A write and one item of a bulk request both need to say the same thing, so
/// neither of them decides it.
pub fn document_complaint(st: &IdxState, source: &Value) -> Option<(String, String, String)> {
    // a date_nanos counts nanoseconds in an i64, which begins in 1970 and runs
    // out in 2262
    for (name, kind) in st.mapping.types.iter() {
        if kind != "date_nanos" {
            continue;
        }
        let Some(value) = source.pointer(&format!("/{}", name.replace('.', "/"))) else {
            continue;
        };
        let texts: Vec<String> = match value {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
            _ => Vec::new(),
        };
        for text in texts {
            // read the text as written: the usual path folds a date through
            // the resolution the index keeps, which is the very thing being
            // checked for
            let Some(dt) = boostcore::time::OffsetDateTime::parse(
                &text,
                &boostcore::time::format_description::well_known::Rfc3339,
            )
            .ok()
            .or_else(|| crate::store::parse_date_lenient(&text)) else {
                continue;
            };
            let nanos = dt.unix_timestamp_nanos();
            let complaint = if dt.year() < 1970 {
                Some(format!(
                    "date[{text}] is before the epoch in 1970 and cannot be stored in \
                     nanosecond resolution"
                ))
            } else if nanos > i64::MAX as i128 {
                Some(format!(
                    "date[{text}] is after 2262-04-11T23:47:16.854775807 and cannot be stored \
                     in nanosecond resolution"
                ))
            } else {
                None
            };
            if let Some(reason) = complaint {
                return Some((
                    "mapper_parsing_exception".into(),
                    format!("failed to parse field [{name}] of type [date_nanos]"),
                    reason,
                ));
            }
        }
    }
    // a nested field is a list of documents of its own, and an index says how
    // many of them one document may carry
    let nested_limit = st.numeric_setting("mapping.nested_objects.limit").unwrap_or(10_000);
    let mut nested_count = 0u64;
    for (name, kind) in st.mapping.types.iter() {
        if kind != "nested" {
            continue;
        }
        if let Some(Value::Array(a)) = source.pointer(&format!("/{}", name.replace('.', "/"))) {
            nested_count += a.len() as u64;
        }
    }
    if nested_count > nested_limit {
        return Some((
            "illegal_argument_exception".into(),
            format!(
                "The number of nested documents has exceeded the allowed limit of \
                 [{nested_limit}]. This limit can be set by changing the \
                 [index.mapping.nested_objects.limit] index level setting."
            ),
            String::new(),
        ));
    }
    // a flat_object keeps whatever object it is given; it is not a place to
    // put a string
    for (name, kind) in st.mapping.types.iter() {
        if kind != "flat_object" {
            continue;
        }
        let Some(value) = source.pointer(&format!("/{}", name.replace('.', "/"))) else {
            continue;
        };
        let ok = match value {
            Value::Object(_) | Value::Null => true,
            Value::Array(a) => a.iter().all(|v| v.is_object() || v.is_null()),
            _ => false,
        };
        if !ok {
            return Some((
                "parsing_exception".into(),
                format!("Failed to parse field [{name}] of type [flat_object]"),
                String::new(),
            ));
        }
    }
    // a completion field filed under contexts has to be given them: without
    // one the value could never be found again
    if let Some(props) = st.mapping.raw.pointer("/properties").and_then(|p| p.as_object()) {
        for (name, def) in props {
            let needs = def
                .get("contexts")
                .and_then(|c| c.as_array())
                .map(|c| c.iter().all(|d| d.get("path").is_none()) && !c.is_empty())
                .unwrap_or(false);
            if !needs {
                continue;
            }
            let Some(value) = source.get(name) else { continue };
            let given = match value {
                Value::Object(o) => o.contains_key("contexts"),
                Value::Array(a) => a
                    .iter()
                    .all(|v| v.as_object().map(|o| o.contains_key("contexts")).unwrap_or(false)),
                _ => false,
            };
            if !given {
                return Some((
                    "mapper_parsing_exception".into(),
                    format!("Contexts are mandatory in context enabled completion field [{name}]"),
                    String::new(),
                ));
            }
        }
    }
    None
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
    if st.setting("blocks.write").as_deref() == Some("true") {
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
    let default_lenient =
        st.setting("mapping.ignore_malformed").map(|v| v == "true").unwrap_or(false);
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
    st.mapping.learn_dynamic(&source);
    // normalized multi-fields are indexed alongside, but never stored
    let mut indexed = crate::store::expand_for_indexing(&source, &st.mapping);
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
    let doc = make_doc(&st.fields, id, indexed, &raw, seq);
    st.queue_op(shard, crate::store::PendingOp::Add(Box::new(doc)));
    st.bytes.fetch_add(raw.len() as u64, std::sync::atomic::Ordering::Relaxed);
    // recorded before it is answered for: the index has it only after a commit
    let routing = st.routing.get(id).cloned();
    st.log_write(id, routing.as_deref(), version, seq, Some(&raw));
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
        "_primary_term": 1,
    });
    Ok((body, status))
}

pub fn delete_doc(st: &mut IdxState, id: &str) -> (Value, StatusCode) {
    let existed = exists_doc(st, id);
    let (version, seq) = st.bump(id, false, existed);
    if existed {
        let shard = st.shard_of_doc(id);
        st.queue_op(shard, crate::store::PendingOp::Delete(id.to_string()));
        st.log_write(id, None, version, seq, None);
        st.note_pending(id, None);
    }
    let body = json!({
        "_index": st.name,
        "_id": id,
        "_version": version,
        "result": if existed { "deleted" } else { "not_found" },
        "_shards": shards_of(st),
        "_seq_no": seq,
        "_primary_term": 1,
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
            // the write takes back the sequence number it was answered with
            if let Some(seq) = rec.get("seq").and_then(|v| v.as_u64()) {
                g.seq_no = seq;
            }
            match rec.get("source") {
                Some(Value::String(raw)) => {
                    let Ok(source) = serde_json::from_str::<Value>(raw) else { continue };
                    match rec.get("routing").and_then(|v| v.as_str()) {
                        Some(r) => {
                            g.routing.insert(id.to_string(), r.to_string());
                        }
                        None => {
                            g.routing.remove(id);
                        }
                    }
                    let _ = write_doc_versioned(
                        &mut g,
                        id,
                        source,
                        "index",
                        Some(raw.clone()),
                        Some(version),
                    );
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

/// `require_alias` says the write is only meant for an alias, so a name that
/// is not one is treated as absent rather than created on the spot.
pub(crate) fn refuse_unless_alias(store: &Store, index: &str, p: &Params) -> Option<Response> {
    let asked = p.get("require_alias").map(|v| v != "false").unwrap_or(false);
    if asked && !store.is_alias(index) {
        return Some(err(
            StatusCode::NOT_FOUND,
            "index_not_found_exception",
            format!(
                "no such index [{index}] and [require_alias] request flag is [true] and [{index}] is not an alias"
            ),
        ));
    }
    None
}

pub async fn index_doc(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    if let Some(r) = refuse_unless_alias(&store, &index, &p) {
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
    if let Some(r) = refuse_unless_alias(&store, &index, &p) {
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
    p: Params,
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
    let st = match store.ensure(&index) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string()),
    };
    let mut g = st.write();
    let id = id.unwrap_or_else(|| g.next_auto_id());
    // A document written with a routing is only reachable by quoting the same
    // routing back, so it has to be remembered -- before the write, because
    // the routing is also what says which shard the write lands on.
    let routed = p.get("routing").filter(|r| !r.is_empty()).cloned();
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

/// Does the routing quoted on a request agree with the one the document was
/// written under? A document reached by the wrong routing is, to the caller,
/// not there at all: in a real cluster the request would have gone to a shard
/// that never held it.
pub(crate) fn routing_matches(st: &IdxState, id: &str, p: &Params) -> bool {
    match st.routing.get(id) {
        Some(have) => p.get("routing").map(|want| want == have).unwrap_or(false),
        None => true,
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
    match read_source_as_asked(&g, &id, &p).filter(|_| routing_matches(&g, &id, &p)) {
        Some(src) => {
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
    if read_source_as_asked(&g, &id, &p).filter(|_| routing_matches(&g, &id, &p)).is_some() {
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
                json!({"query_string": qs})
            }
            None => json!({"match_all": {}}),
        },
    };
    let scoped = json!({"bool": {"must": [q], "filter": [{"ids": {"values": [id]}}]}});
    let probe = json!({"query": scoped, "size": 1});
    let matched = crate::search::run(&store, &name, &probe, &Params::new())
        .map(|o| o.total > 0)
        .unwrap_or(false);

    let mut out = json!({
        "_index": name,
        "_id": id,
        "matched": matched,
        "explanation": {
            "value": if matched { 1.0 } else { 0.0 },
            "description": if matched { "match" } else { "no match" },
            "details": []
        }
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
