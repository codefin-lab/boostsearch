//! Writing, reading and deleting one document, and the bulk of many.

use super::*;

/// `_termvectors` -- what a document's text became once analysed.
///
/// The terms are recovered by analysing the stored source again rather than
/// from a second index of offsets, which is the same ground the highlighter
/// stands on. Document frequency is counted against the index, so it is the
/// real one rather than a guess.
pub async fn termvectors(
    State(store): State<Store>,
    path: Path<Vec<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let parts = path.0;
    let index = parts.first().cloned().unwrap_or_default();
    let id = parts.get(1).cloned();
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let Some(st) = store.get(&index) else { return no_such_index(&index) };
    let g = st.read();
    let id = id.or_else(|| body.get("_id").and_then(|v| v.as_str().map(|s| s.into())));
    let Some(id) = id else {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: id is missing;",
        );
    };
    let source = read_source_as_asked(&g, &id, &p);
    let Some(source) = source else {
        return respond(
            &p,
            json!({
                "_index": g.name, "_id": id, "_version": 0, "found": false, "took": 0,
            }),
        );
    };
    let want_stats = body
        .get("term_statistics")
        .and_then(|v| v.as_bool())
        .or_else(|| p.get("term_statistics").map(|v| v == "true"))
        .unwrap_or(false);
    let only: Option<Vec<String>> = body
        .get("fields")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .or_else(|| p.get("fields").map(|f| f.split(',').map(|s| s.trim().to_string()).collect()));

    let fields = term_vectors_of(&g, &source, want_stats, only.as_deref());
    respond(
        &p,
        json!({
            "_index": g.name, "_id": id, "_version": g.version_of(&id),
            "found": true, "took": 0, "term_vectors": fields,
        }),
    )
}

/// The terms each field of a document became once analysed.
pub(crate) fn term_vectors_of(
    g: &IdxState,
    source: &Value,
    want_stats: bool,
    only: Option<&[String]>,
) -> Value {
    let mut fields = serde_json::Map::new();
    let Some(obj) = source.as_object() else { return Value::Object(fields) };
    for (name, value) in obj {
        if only.map(|f| !f.iter().any(|w| w == name)).unwrap_or(false) {
            continue;
        }
        let Some(text) = value.as_str() else { continue };
        let spans = crate::query::analyze_spans(&g.index, text, None);
        if spans.is_empty() {
            continue;
        }
        // group the occurrences by the term they are of
        let mut terms: std::collections::BTreeMap<String, Vec<(usize, usize, usize)>> =
            std::collections::BTreeMap::new();
        for (t, pos, from, to) in spans {
            terms.entry(t).or_default().push((pos, from, to));
        }
        let searcher = g.reader.searcher();
        let mut out = serde_json::Map::new();
        let mut sum_doc_freq = 0u64;
        let mut sum_ttf = 0u64;
        for (term, spots) in &terms {
            let mut entry = json!({
                "term_freq": spots.len(),
                "tokens": spots.iter().map(|(pos, from, to)| json!({
                    "position": pos, "start_offset": from, "end_offset": to,
                })).collect::<Vec<_>>(),
            });
            if want_stats {
                // how many documents hold this term, counted rather than assumed
                let ctx = crate::query::Ctx {
                    fields: &g.fields,
                    mapping: &g.mapping,
                    index: &g.index,
                    max_terms_count: g.max_terms_count(),
                    max_regex_length: g.max_regex_length(),
                    allow_expensive: true,
                    observed_kinds: &g.observed_kinds,
                    kinds_complete: g.kinds_complete,
                    stats: &g.stats,
                };
                let freq = crate::query::build(&ctx, &json!({"match": {name.clone(): term}}))
                    .ok()
                    .and_then(|q| searcher.search(&q, &boostcore::collector::Count).ok())
                    .unwrap_or(1) as u64;
                entry["doc_freq"] = json!(freq);
                entry["ttf"] = json!(spots.len());
                sum_doc_freq += freq;
                sum_ttf += spots.len() as u64;
            }
            out.insert(term.clone(), entry);
        }
        let mut field = json!({"terms": Value::Object(out)});
        if want_stats {
            field["field_statistics"] = json!({
                "sum_doc_freq": sum_doc_freq,
                "doc_count": searcher.num_docs(),
                "sum_ttf": sum_ttf,
            });
        }
        fields.insert(name.clone(), field);
    }
    Value::Object(fields)
}

/// `_mtermvectors` -- term vectors for several documents at once.
pub async fn mtermvectors(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let default_index = index.map(|Path(i)| i);
    let docs: Vec<Value> = match body.get("docs").and_then(|v| v.as_array()) {
        Some(a) => a.clone(),
        None => {
            // `ids` may be written in the body or on the URL
            let listed: Vec<Value> = body
                .get("ids")
                .and_then(|v| v.as_array())
                .cloned()
                .or_else(|| {
                    p.get("ids")
                        .filter(|v| !v.is_empty())
                        .map(|v| v.split(',').map(|s| json!(s.trim())).collect())
                })
                .unwrap_or_default();
            listed.into_iter().map(|id| json!({"_id": id})).collect()
        }
    };
    // the camel-cased spellings, and the ones that moved onto the request,
    // are no longer read from a document
    for d in &docs {
        for gone in ["version", "versionType", "version_type", "_version", "routing", "_routing"] {
            if d.get(gone).is_some() {
                return err(
                    StatusCode::BAD_REQUEST,
                    "action_request_validation_exception",
                    format!("Validation Failed: 1: unknown field [{gone}];"),
                );
            }
        }
    }
    let mut out = Vec::new();
    for d in docs {
        let idx = d
            .get("_index")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .or_else(|| default_index.clone())
            .unwrap_or_default();
        let id = d.get("_id").map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        });
        let Some(id) = id else { continue };
        let Some(st) = store.get(&idx) else {
            let reason = format!("no such index [{idx}]");
            let cause = json!({
                "type": "index_not_found_exception", "reason": reason,
                "index": idx, "resource.type": "index_expression",
                "resource.id": idx, "index_uuid": "_na_",
            });
            let mut error = json!({
                "type": "index_not_found_exception", "reason": reason,
                "index": idx, "resource.type": "index_expression",
                "resource.id": idx, "index_uuid": "_na_",
                "root_cause": [cause],
            });
            add_stack_trace(&mut error, &p, "mtermvectors");
            out.push(json!({"_index": idx, "_id": id, "found": false, "error": error}));
            continue;
        };
        let g = st.read();
        match read_source_as_asked(&g, &id, &p) {
            Some(src) => {
                let want_stats = body
                    .get("term_statistics")
                    .and_then(|v| v.as_bool())
                    .or_else(|| p.get("term_statistics").map(|v| v == "true"))
                    .unwrap_or(false);
                out.push(json!({
                    "_index": g.name, "_id": id, "_version": g.version_of(&id),
                    "found": true, "took": 0,
                    "term_vectors": term_vectors_of(&g, &src, want_stats, None),
                }))
            }
            None => out.push(json!({
                "_index": g.name, "_id": id, "found": false, "took": 0,
            })),
        }
    }
    respond(&p, json!({"docs": out}))
}

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

pub async fn bulk(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    _headers: HeaderMap,
    body: String,
) -> Response {
    let default_index = index.map(|Path(i)| i);
    let mut items = Vec::new();
    let mut errors = false;
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
        // keep the number of live writers bounded across indices
        if !g_has_writer(&st) {
            store.note_writer_opened(&idx);
        }
        let mut g = st.write();
        let id_was_given = id_opt.is_some();
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
                if let Some(pipe) = meta.get("pipeline").and_then(|v| v.as_str()) {
                    errors = true;
                    items.push(json!({ op.clone(): {
                        "_index": idx, "_id": id, "status": 400,
                        "error": {
                            "type": "illegal_argument_exception",
                            "reason": format!("pipeline with id [{pipe}] does not exist")
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
                match meta.get("routing").and_then(|v| v.as_str()).filter(|r| !r.is_empty()) {
                    Some(r) => {
                        g.routing.insert(id.clone(), r.to_string());
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
                            let ups = ups.clone();
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
    axum::Json(json!({"took": 0, "errors": errors, "items": items})).into_response()
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
        let g = st.read();
        let routing_ok = match g.routing.get(&id) {
            Some(have) => want_routing.as_deref() == Some(have.as_str()),
            None => true,
        };
        match read_source_as_asked(&g, &id, &p).filter(|_| routing_ok) {
            Some(src) => {
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

pub async fn update_doc(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    if let Some(r) = refuse_unless_alias(&store, &index, &p) {
        return r;
    }
    let patch: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    const UPDATE_KEYS: &[&str] = &[
        "doc",
        "upsert",
        "doc_as_upsert",
        "detect_noop",
        "_source",
        "script",
        "scripted_upsert",
        "if_seq_no",
        "if_primary_term",
    ];
    if let Some(o) = patch.as_object() {
        for k in o.keys() {
            if !UPDATE_KEYS.contains(&k.as_str()) {
                // OpenSearch offers a spelling hint for near-misses
                let hint = if k.len() == 3 && k.starts_with('d') && k.ends_with('c') {
                    " did you mean [doc]?"
                } else {
                    ""
                };
                return err(
                    StatusCode::BAD_REQUEST,
                    "x_content_parse_exception",
                    format!("[UpdateRequest] unknown field [{k}]{hint}"),
                );
            }
        }
    }
    // an update carrying an upsert creates the index, the way OpenSearch does
    let has_upsert = patch.get("upsert").is_some()
        || patch.get("doc_as_upsert").and_then(|v| v.as_bool()).unwrap_or(false);
    let st = if has_upsert {
        match store.ensure(&index) {
            Ok(s) => s,
            Err(e) => {
                return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string());
            }
        }
    } else {
        match store.get(&index) {
            Some(s) => s,
            None => return no_such_index(&index),
        }
    };
    let mut g = st.write();
    // the wrong routing reaches nothing, so there is no document to update
    let existing = read_source(&g, &id).filter(|_| routing_matches(&g, &id, &p));
    // `if_seq_no` makes the write conditional on the document not having moved
    // since the caller read it. A document that is not there at all is a
    // different complaint, and is left to the missing-document path below.
    if let (Some(want), true) =
        (p.get("if_seq_no").and_then(|v| v.parse::<u64>().ok()), existing.is_some())
    {
        let have = read_seq(&g, &id).unwrap_or(0);
        if have != want {
            return err(
                StatusCode::CONFLICT,
                "version_conflict_engine_exception",
                format!(
                    "[{id}]: version conflict, required seqNo [{want}], primary term [1]. \
                     current document has seqNo [{have}] and primary term [1]"
                ),
            );
        }
    }
    let detect_noop = patch.get("detect_noop").and_then(|v| v.as_bool()).unwrap_or(true);
    let doc_as_upsert = patch.get("doc_as_upsert").and_then(|v| v.as_bool()).unwrap_or(false);

    let (next, result) = match (existing.clone(), patch.get("doc")) {
        (Some(base), Some(d)) => {
            let mut merged = base.clone();
            merge_into(&mut merged, d);
            if detect_noop && merged == base {
                // the write guard is already held here; taking a read on the
                // same lock would wait for itself
                g.noop_updates.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                (base, "noop")
            } else {
                (merged, "updated")
            }
        }
        (Some(base), None) => (base, "noop"),
        (None, doc) => {
            let ups = patch
                .get("upsert")
                .cloned()
                .or_else(|| if doc_as_upsert { doc.cloned() } else { None });
            match ups {
                Some(u) => (u, "created"),
                None => {
                    return err(
                        StatusCode::NOT_FOUND,
                        "document_missing_exception",
                        format!("[{id}]: document missing"),
                    );
                }
            }
        }
    };

    let mut body_out = if result == "noop" {
        let version = g.version_of(&id);
        json!({
            "_index": g.name, "_id": id, "_version": version, "result": "noop",
            "_shards": {"total": 0, "successful": 0, "failed": 0},
            // an update that changed nothing still reports where the document
            // stands, which is where it already stood
            "_seq_no": read_seq(&g, &id).unwrap_or(0), "_primary_term": 1,
        })
    } else {
        match write_doc(&mut g, &id, next.clone(), "index") {
            Ok((mut b, _)) => {
                b["result"] = json!(result);
                b
            }
            Err(r) => return r,
        }
    };

    let sel = patch.get("_source").cloned().or_else(|| source_selector_from_params(&p));
    if let Some(sel) = sel.as_ref().filter(|v| **v != json!(false)) {
        body_out["get"] = json!({"_source": apply_source_selector(&next, sel), "found": true});
    }
    // a routing given on the update is the one the document keeps
    match p.get("routing").filter(|r| !r.is_empty()) {
        Some(r) => {
            g.routing.insert(id.clone(), r.clone());
            body_out["_routing"] = json!(r);
        }
        None => {
            if let Some(r) = g.routing.get(&id) {
                body_out["_routing"] = json!(r);
            }
        }
    }
    let shard = g.shard_of_doc(&id);
    maybe_refresh(&mut g, &p, Some(shard));
    note_forced_refresh(&mut body_out, &p);
    let status = if result == "created" { StatusCode::CREATED } else { StatusCode::OK };
    (status, axum::Json(body_out)).into_response()
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
