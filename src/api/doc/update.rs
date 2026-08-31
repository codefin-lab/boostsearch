//! Changing a document that is already there.

use super::*;

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
