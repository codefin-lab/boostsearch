//! REST surface: index lifecycle and document CRUD.
//!
//! Response envelopes follow OpenSearch exactly -- `result`, `_version`,
//! `_shards`, `_seq_no` and friends are asserted on directly by the YAML suite.

use crate::store::{IdxState, Store, make_doc};
use anyhow::Result;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::collections::HashMap;
use tantivy::collector::TopDocs;
use tantivy::query::TermQuery;
use tantivy::schema::{IndexRecordOption, Term, Value as _};
use tantivy::TantivyDocument;

pub type Params = HashMap<String, String>;

pub fn err(status: StatusCode, kind: &str, reason: impl Into<String>) -> Response {
    let reason = reason.into();
    (
        status,
        axum::Json(json!({
            "error": {"type": kind, "reason": reason, "root_cause": [{"type": kind, "reason": reason}]},
            "status": status.as_u16()
        })),
    )
        .into_response()
}

/// An error that quotes an inner cause, the shape the search API uses.
pub fn err_caused_by(kind: &str, reason: &str, cause: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({
            "error": {
                "type": kind,
                "reason": reason,
                "root_cause": [{"type": kind, "reason": reason}],
                "caused_by": {"type": "illegal_argument_exception", "reason": cause}
            },
            "status": 400
        })),
    )
        .into_response()
}

pub fn no_such_index(name: &str) -> Response {
    let reason = format!("no such index [{name}]");
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({
            "error": {
                "type": "index_not_found_exception",
                "reason": reason,
                "index": name,
                "resource.type": "index_or_alias",
                "resource.id": name,
                "index_uuid": "_na_",
                "root_cause": [{"type": "index_not_found_exception", "reason": reason, "index": name}]
            },
            "status": 404
        })),
    )
        .into_response()
}

fn shards() -> Value {
    json!({"total": 2, "successful": 1, "failed": 0})
}

fn flag(p: &Params, key: &str) -> bool {
    matches!(p.get(key).map(|s| s.as_str()), Some("true") | Some("") | Some("wait_for"))
}

fn ignore_unavailable(p: &Params) -> bool {
    // `allow_no_indices=false` overrides ignore_unavailable: an expression that
    // resolves to nothing is still an error
    if p.get("allow_no_indices").map(|v| v == "false").unwrap_or(false) {
        return false;
    }
    flag(p, "ignore_unavailable")
}

/// `?ignore=404` suppresses the error the test would otherwise catch.
fn ignored(p: &Params, status: StatusCode) -> bool {
    p.get("ignore")
        .map(|v| v.split(',').any(|c| c.trim() == status.as_u16().to_string()))
        .unwrap_or(false)
}

// ---------------------------------------------------------------- index CRUD

pub async fn create_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(_p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = if body.trim().is_empty() {
        json!({})
    } else {
        match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => return err(StatusCode::BAD_REQUEST, "parse_exception", e.to_string()),
        }
    };
    if store.exists(&index) {
        return err(
            StatusCode::BAD_REQUEST,
            "resource_already_exists_exception",
            format!("index [{index}] already exists"),
        );
    }
    // adaptive shard selection only makes sense on an append-only index
    let setting = |k: &str| -> Option<String> {
        body.pointer(&format!("/settings/index/{k}"))
            .or_else(|| body.pointer(&format!("/settings/index.{k}")))
            .map(|v| v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string()))
    };
    if setting("bulk.adaptive_shard_selection.enabled").as_deref() == Some("true")
        && setting("append_only.enabled").as_deref() != Some("true")
    {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "index [{index}] is not append-only index, bulk adaptive shard selection is \
                 enabled, which is not supported. Please disable bulk adaptive shard selection \
                 or set index to append-only index."
            ),
        );
    }
    match store.create(&index, &body) {
        Ok(()) => axum::Json(json!({
            "acknowledged": true, "shards_acknowledged": true, "index": index
        }))
        .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string()),
    }
}

pub async fn delete_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let targets = store.resolve(&index);
    if targets.is_empty() && !index.contains('*') && !ignore_unavailable(&p) {
        return no_such_index(&index);
    }
    store.delete(&index);
    axum::Json(json!({"acknowledged": true})).into_response()
}

pub async fn index_exists(State(store): State<Store>, Path(index): Path<String>) -> Response {
    if store.resolve(&index).is_empty() {
        StatusCode::NOT_FOUND.into_response()
    } else {
        StatusCode::OK.into_response()
    }
}

pub async fn refresh_all(State(store): State<Store>) -> Response {
    for n in store.names() {
        if let Some(st) = store.get(&n) {
            let _ = st.write().refresh();
        }
    }
    axum::Json(json!({"_shards": shards()})).into_response()
}

pub async fn refresh_index(State(store): State<Store>, Path(index): Path<String>) -> Response {
    let targets = store.resolve(&index);
    if targets.is_empty() {
        return no_such_index(&index);
    }
    for n in targets {
        if let Some(st) = store.get(&n) {
            let _ = st.write().refresh();
        }
    }
    axum::Json(json!({"_shards": shards()})).into_response()
}

// ------------------------------------------------------------------ mappings

fn mapping_view(st: &IdxState) -> Value {
    let m = if st.mapping.raw.is_null() { json!({}) } else { st.mapping.raw.clone() };
    json!({"mappings": m})
}

pub async fn get_mapping(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_else(|| "_all".into());
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !ignore_unavailable(&p) {
        return no_such_index(&expr);
    }
    let mut out = serde_json::Map::new();
    for n in targets {
        if let Some(st) = store.get(&n) {
            out.insert(n, mapping_view(&st.read()));
        }
    }
    axum::Json(Value::Object(out)).into_response()
}

pub async fn put_mapping(
    State(store): State<Store>,
    Path(index): Path<String>,
    body: String,
) -> Response {
    let body: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let targets = store.resolve(&index);
    if targets.is_empty() {
        return no_such_index(&index);
    }
    for n in targets {
        if let Some(st) = store.get(&n) {
            let mut g = st.write();
            g.mapping.merge(&body);
        }
    }
    axum::Json(json!({"acknowledged": true})).into_response()
}

/// Settings OpenSearch reports under `defaults` when asked for them. Only the
/// handful the conformance suite reads are modelled.
fn default_settings() -> Value {
    json!({"index": {
        "refresh_interval": "1s",
        "max_result_window": "10000",
        "number_of_routing_shards": "1",
        "codec": "default",
        "auto_expand_replicas": "false",
        "max_inner_result_window": "100",
        "max_rescore_window": "10000",
        "query": {"default_field": ["*"]},
    }})
}

/// `flat_settings=true` renders `{"index":{"a":1}}` as `{"index.a":1}`.
fn flatten_settings(v: &Value, prefix: &str, out: &mut serde_json::Map<String, Value>) {
    match v {
        Value::Object(o) => {
            for (k, child) in o {
                let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten_settings(child, &path, out);
            }
        }
        leaf => {
            out.insert(prefix.to_string(), leaf.clone());
        }
    }
}

fn settings_view(raw: &Value, name: Option<&str>, flat: bool) -> Value {
    let mut flat_map = serde_json::Map::new();
    flatten_settings(raw, "", &mut flat_map);
    if let Some(name) = name {
        if name != "_all" && name != "*" {
            let pats: Vec<regex::Regex> =
                name.split(',').map(|p| crate::store::wildcard_to_regex(p.trim())).collect();
            flat_map.retain(|k, _| pats.iter().any(|re| re.is_match(k)));
        }
    }
    if flat {
        return Value::Object(flat_map);
    }
    // rebuild the nested shape from whatever survived the filter
    let mut nested = json!({});
    for (k, v) in flat_map {
        let mut cur = &mut nested;
        let segs: Vec<&str> = k.split('.').collect();
        for seg in &segs[..segs.len() - 1] {
            cur = cur
                .as_object_mut()
                .unwrap()
                .entry(seg.to_string())
                .or_insert_with(|| json!({}));
            if !cur.is_object() {
                *cur = json!({});
            }
        }
        cur.as_object_mut().unwrap().insert(segs[segs.len() - 1].to_string(), v);
    }
    nested
}

pub async fn get_settings(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    settings_response(store, index.map(|Path(i)| i), None, p)
}

pub async fn get_settings_all_named(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    settings_response(store, None, Some(name), p)
}

pub async fn get_settings_named(
    State(store): State<Store>,
    Path((index, name)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    settings_response(store, Some(index), Some(name), p)
}

fn settings_response(
    store: Store,
    index: Option<String>,
    name: Option<String>,
    p: Params,
) -> Response {
    let expr = index.unwrap_or_else(|| "_all".into());
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !ignore_unavailable(&p) {
        return no_such_index(&expr);
    }
    let flat = flag(&p, "flat_settings");
    let mut out = serde_json::Map::new();
    for n in targets {
        let Some(st) = store.get(&n) else { continue };
        let raw = st.read().effective_settings();
        let mut entry = json!({ "settings": settings_view(&raw, name.as_deref(), flat) });
        if flag(&p, "include_defaults") {
            entry["defaults"] = settings_view(&default_settings(), name.as_deref(), flat);
        }
        out.insert(n.clone(), entry);
    }
    axum::Json(Value::Object(out)).into_response()
}

// -------------------------------------------------------------- document CRUD

/// Read a document's `_source`, honouring uncommitted writes so GET is realtime.
fn source_enabled(st: &IdxState) -> bool {
    st.mapping
        .raw
        .get("_source")
        .and_then(|s| s.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

pub fn read_source(st: &IdxState, id: &str) -> Option<Value> {
    if let Some(p) = st.pending.get(id) {
        return p.as_ref().and_then(|raw| serde_json::from_str(raw).ok());
    }
    // the realtime reader sees flushed-but-unrefreshed writes
    let searcher = st.realtime.searcher();
    let q = TermQuery::new(Term::from_field_text(st.fields.id, id), IndexRecordOption::Basic);
    let hits = searcher.search(&q, &TopDocs::with_limit(1).order_by_score()).ok()?;
    let (_, addr) = hits.first()?;
    let doc: TantivyDocument = searcher.doc(*addr).ok()?;
    let raw = doc.get_first(st.fields.source)?.as_str()?.to_string();
    serde_json::from_str(&raw).ok()
}

pub fn exists_doc(st: &IdxState, id: &str) -> bool {
    st.is_live(id)
}

/// Write one document. `op_type == "create"` refuses to overwrite.
pub fn append_only(st: &IdxState) -> bool {
    st.setting("append_only.enabled").map(|v| v == "true").unwrap_or(false)
}

pub fn write_doc(
    st: &mut IdxState,
    id: &str,
    source: Value,
    op_type: &str,
) -> std::result::Result<(Value, StatusCode), Response> {
    write_doc_raw(st, id, source, op_type, None)
}

/// `raw` is the document exactly as the client sent it; passing it through
/// avoids re-serialising a tree we only just parsed.
pub fn write_doc_raw(
    st: &mut IdxState,
    id: &str,
    source: Value,
    op_type: &str,
    raw: Option<String>,
) -> std::result::Result<(Value, StatusCode), Response> {
    let existed = exists_doc(st, id);
    if op_type == "create" && existed {
        return Err(err(
            StatusCode::CONFLICT,
            "version_conflict_engine_exception",
            format!("[{id}]: version conflict, document already exists"),
        ));
    }
    let (version, seq) = st.bump(id, true, existed);
    st.observe(&source);
    // deleting is only needed when something is actually there to replace;
    // a bulk load of new documents should not queue a delete per document
    if existed {
        let term = Term::from_field_text(st.fields.id, id);
        if let Ok(w) = st.writer() {
            w.delete_term(term);
        }
    }
    let raw = raw.unwrap_or_else(|| source.to_string());
    let doc = make_doc(&st.fields, id, source, &raw);
    match st.writer() {
        Ok(w) => {
            if let Err(e) = w.add_document(doc) {
                return Err(err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "index_exception",
                    e.to_string(),
                ));
            }
        }
        Err(e) => {
            return Err(err(StatusCode::INTERNAL_SERVER_ERROR, "index_exception", e.to_string()));
        }
    }
    st.note_pending(id, Some(raw));
    let status = if existed { StatusCode::OK } else { StatusCode::CREATED };
    let body = json!({
        "_index": st.name,
        "_id": id,
        "_version": version,
        "result": if existed { "updated" } else { "created" },
        "_shards": shards(),
        "_seq_no": seq,
        "_primary_term": 1,
    });
    Ok((body, status))
}

pub fn delete_doc(st: &mut IdxState, id: &str) -> (Value, StatusCode) {
    let existed = exists_doc(st, id);
    let (version, seq) = st.bump(id, false, existed);
    if existed {
        let term = Term::from_field_text(st.fields.id, id);
        if let Ok(w) = st.writer() {
            w.delete_term(term);
        }
        st.note_pending(id, None);
    }
    let body = json!({
        "_index": st.name,
        "_id": id,
        "_version": version,
        "result": if existed { "deleted" } else { "not_found" },
        "_shards": shards(),
        "_seq_no": seq,
        "_primary_term": 1,
    });
    (body, if existed { StatusCode::OK } else { StatusCode::NOT_FOUND })
}

fn maybe_refresh(st: &mut IdxState, p: &Params) {
    if flag(p, "refresh") {
        let _ = st.refresh();
    }
}

pub async fn index_doc(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    do_index(store, index, Some(id), p, body, "index").await
}

pub async fn index_doc_auto(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
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

async fn do_index(
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
    match write_doc(&mut g, &id, source, &op_type) {
        Ok((body, status)) => {
            maybe_refresh(&mut g, &p);
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
    let Some(st) = store.get(&index) else {
        return if ignored(&p, StatusCode::NOT_FOUND) {
            (StatusCode::NOT_FOUND, axum::Json(json!({"_index": index, "_id": id, "found": false})))
                .into_response()
        } else {
            no_such_index(&index)
        };
    };
    let g = st.read();
    match read_source(&g, &id) {
        Some(src) => {
            let fields = stored_fields(&src, &p);
            let mut body = json!({
                "_index": g.name, "_id": id,
                "_version": g.version_of(&id),
                "_seq_no": 0, "_primary_term": 1,
                "found": true,
            });
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
) -> Response {
    let Some(st) = store.get(&index) else { return StatusCode::NOT_FOUND.into_response() };
    let g = st.read();
    if exists_doc(&g, &id) { StatusCode::OK.into_response() } else { StatusCode::NOT_FOUND.into_response() }
}

pub async fn get_source(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    let Some(st) = store.get(&index) else {
        return if ignored(&p, StatusCode::NOT_FOUND) {
            (StatusCode::NOT_FOUND, axum::Json(json!({}))).into_response()
        } else {
            no_such_index(&index)
        };
    };
    let g = st.read();
    if !source_enabled(&g) {
        return err(
            StatusCode::NOT_FOUND,
            "illegal_argument_exception",
            format!("fields [_source] are disabled in the mappings for index [{}]", g.name),
        );
    }
    match read_source(&g, &id) {
        Some(src) => axum::Json(filter_source_params(&src, &p)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"_index": g.name, "_id": id, "found": false})),
        )
            .into_response(),
    }
}

pub async fn delete_doc_route(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    let Some(st) = store.get(&index) else { return no_such_index(&index) };
    let mut g = st.write();
    let (body, status) = delete_doc(&mut g, &id);
    maybe_refresh(&mut g, &p);
    (status, axum::Json(body)).into_response()
}

// ---------------------------------------------------------------------- bulk

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
    // each document and building its tantivy form -- can run across cores.
    struct Op<'a> {
        op: String,
        meta: Value,
        index: String,
        id: Option<String>,
        doc_line: Option<&'a str>,
    }
    let mut ops: Vec<Op> = Vec::new();
    let mut lines = body.lines().filter(|l| !l.trim().is_empty());
    while let Some(action_line) = lines.next() {
        let action: Value = match serde_json::from_str(action_line) {
            Ok(v) => v,
            Err(e) => {
                return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string());
            }
        };
        let Some((op, meta)) = action.as_object().and_then(|o| o.iter().next()) else {
            return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", "malformed action");
        };
        let op = op.clone();
        let idx = meta
            .get("_index")
            .and_then(scalar_str)
            .or_else(|| default_index.clone());
        let Some(idx) = idx else {
            return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", "missing index");
        };
        let id_opt = meta.get("_id").and_then(scalar_str);
        let doc_line = if op == "delete" { None } else { lines.next() };
        ops.push(Op { op, meta: meta.clone(), index: idx, id: id_opt, doc_line });
    }

    // Parse and build documents in parallel; nothing here touches shared state.
    let prepared: Vec<Option<std::result::Result<(Value, String), String>>> = {
        use rayon::prelude::*;
        ops.par_iter()
            .map(|o| {
                o.doc_line.map(|l| {
                    serde_json::from_str::<Value>(l)
                        .map(|v| (v, l.trim().to_string()))
                        .map_err(|e| e.to_string())
                })
            })
            .collect()
    };

    // consume the prepared documents rather than cloning them back out
    for (o, prep) in ops.into_iter().zip(prepared.into_iter()) {
        let op = o.op;
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

        let st = match store.ensure(&idx) {
            Ok(s) => s,
            Err(e) => {
                return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string());
            }
        };
        if !touched.contains(&idx) {
            touched.push(idx.clone());
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
                let src = source.unwrap_or_else(|| json!({}));
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
                let patch = source.unwrap_or_else(|| json!({}));
                let doc = patch.get("doc").cloned();
                match (existing, doc) {
                    (Some(mut base), Some(d)) => {
                        merge_into(&mut base, &d);
                        match write_doc(&mut g, &id, base.clone(), "index") {
                            Ok((body, _)) => {
                                let mut b = body;
                                b["result"] = json!("updated");
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
                        let as_upsert = patch
                            .get("doc_as_upsert")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
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
                            json!({"update": {
                                "_index": idx, "_id": id, "status": 404,
                                "error": {"type": "document_missing_exception",
                                          "reason": format!("[{id}]: document missing")}
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

    if flag(&p, "refresh") {
        for n in touched {
            if let Some(st) = store.get(&n) {
                let _ = st.write().refresh();
            }
        }
    }
    axum::Json(json!({"took": 0, "errors": errors, "items": items})).into_response()
}

// ------------------------------------------------------------ source filtering

pub fn merge_into(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                match b.get_mut(k) {
                    Some(slot) if slot.is_object() && v.is_object() => merge_into(slot, v),
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, p) => *b = p.clone(),
    }
}

fn as_list(p: &Params, key: &str) -> Option<Vec<String>> {
    p.get(key).map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

fn filter_source_params(src: &Value, p: &Params) -> Value {
    if let Some(v) = p.get("_source") {
        if v == "false" {
            return json!(null);
        }
        if v != "true" {
            return crate::source::filter(src, &as_list(p, "_source").unwrap_or_default(), &[]);
        }
    }
    let inc = as_list(p, "_source_includes").or_else(|| as_list(p, "_source_include"));
    let exc = as_list(p, "_source_excludes").or_else(|| as_list(p, "_source_exclude"));
    if inc.is_none() && exc.is_none() {
        return src.clone();
    }
    crate::source::filter(src, &inc.unwrap_or_default(), &exc.unwrap_or_default())
}

/// `_source` as it can appear in a request body: bool, pattern, list, or
/// an object with includes/excludes.
pub fn apply_source_selector(src: &Value, sel: &Value) -> Value {
    match sel {
        Value::Bool(true) | Value::Null => src.clone(),
        Value::Bool(false) => Value::Null,
        Value::String(pat) => crate::source::filter(src, &[pat.clone()], &[]),
        Value::Array(items) => {
            let inc: Vec<String> =
                items.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
            crate::source::filter(src, &inc, &[])
        }
        Value::Object(o) => {
            let pick = |k1: &str, k2: &str| -> Vec<String> {
                let v = o.get(k1).or_else(|| o.get(k2));
                match v {
                    Some(Value::String(s)) => vec![s.clone()],
                    Some(Value::Array(a)) => {
                        a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()
                    }
                    _ => vec![],
                }
            };
            crate::source::filter(src, &pick("includes", "include"), &pick("excludes", "exclude"))
        }
        _ => src.clone(),
    }
}

/// The `_source*` query-string family, as a selector value.
fn source_selector_from_params(p: &Params) -> Option<Value> {
    if let Some(v) = p.get("_source") {
        return Some(match v.as_str() {
            "true" => json!(true),
            "false" => json!(false),
            other if other.contains(',') => {
                json!(other.split(',').map(|s| s.trim()).collect::<Vec<_>>())
            }
            other => json!(other),
        });
    }
    let inc = as_list(p, "_source_includes").or_else(|| as_list(p, "_source_include"));
    let exc = as_list(p, "_source_excludes").or_else(|| as_list(p, "_source_exclude"));
    if inc.is_none() && exc.is_none() {
        return None;
    }
    Some(json!({
        "includes": inc.unwrap_or_default(),
        "excludes": exc.unwrap_or_default(),
    }))
}

/// `stored_fields` is answered from `_source`; every value comes back as a list.
fn wants_source_via_stored_fields(p: &Params) -> bool {
    p.get("stored_fields")
        .map(|s| s.split(',').any(|f| f.trim() == "_source"))
        .unwrap_or(false)
}

fn stored_fields(src: &Value, p: &Params) -> Option<Value> {
    let spec = p.get("stored_fields")?;
    if spec == "_none_" || spec.is_empty() {
        return None;
    }
    let mut out = serde_json::Map::new();
    for name in spec.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if name == "_source" {
            continue;
        }
        let picked = crate::source::filter(src, &[name.to_string()], &[]);
        if let Some(v) = flat_lookup(&picked, name) {
            let arr = match v {
                Value::Array(a) => Value::Array(a),
                other => Value::Array(vec![other]),
            };
            out.insert(name.to_string(), arr);
        }
    }
    if out.is_empty() { None } else { Some(Value::Object(out)) }
}

fn flat_lookup(v: &Value, path: &str) -> Option<Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur.clone())
}

pub async fn not_ported() -> Response {
    err(StatusCode::NOT_IMPLEMENTED, "not_implemented_exception", "not ported yet")
}

pub fn _unused(_: Result<()>) {}

// -------------------------------------------------------------------- search

pub fn source_selector_from_params_pub(p: &Params) -> Option<Value> {
    source_selector_from_params(p)
}

/// Every JSON response goes through here so `filter_path` works uniformly.
pub fn respond(p: &Params, v: Value) -> Response {
    match p.get("filter_path") {
        Some(spec) if !spec.is_empty() => {
            axum::Json(crate::source::filter_path(&v, spec)).into_response()
        }
        _ => axum::Json(v).into_response(),
    }
}

fn parse_body(body: &str) -> std::result::Result<Value, Response> {
    if body.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(body)
        .map_err(|e| err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string()))
}

/// Query-string forms of the search request that the suite exercises.
fn fold_params_into_body(body: &mut Value, p: &Params) {
    if let Some(q) = p.get("q") {
        if body.get("query").is_none() {
            let mut qs = json!({"query": q});
            if let Some(df) = p.get("df").or_else(|| p.get("default_field")) {
                qs["default_field"] = json!(df);
            }
            if let Some(op) = p.get("default_operator") {
                qs["default_operator"] = json!(op.to_lowercase());
            }
            body["query"] = json!({ "query_string": qs });
        }
    }
    for key in ["from", "size", "track_total_hits"] {
        if let (Some(v), None) = (p.get(key), body.get(key)) {
            body[key] = match v.as_str() {
                "true" => json!(true),
                "false" => json!(false),
                s => s.parse::<i64>().map(|n| json!(n)).unwrap_or(json!(s)),
            };
        }
    }
    if let Some(s) = p.get("sort") {
        if body.get("sort").is_none() {
            let items: Vec<Value> = s
                .split(',')
                .map(|part| match part.split_once(':') {
                    Some((f, o)) => json!({ f: o }),
                    None => json!(part),
                })
                .collect();
            body["sort"] = Value::Array(items);
        }
    }
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
    match crate::search::run(&store, &expr, &body, &p) {
        Ok(out) => respond(&p, crate::search::envelope(out, &body, &p)),
        Err(r) => r,
    }
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
            respond(&p, json!({
                "count": out.total,
                "_shards": {"total": n, "successful": n, "skipped": skipped, "failed": 0}
            }))
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
                responses.push(json!({
                    "error": {
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
                    },
                    "status": 404
                }));
            }
        }
    }
    respond(&p, json!({"took": 1, "responses": responses}))
}

// ----------------------------------------------------------------------- mget

/// OpenSearch refuses a field whose name is nothing but dots.
fn dotted_only_field(v: &Value) -> Option<String> {
    match v {
        Value::Object(o) => {
            for (k, child) in o {
                if !k.is_empty() && k.chars().all(|c| c == '.') {
                    return Some(k.clone());
                }
                if let Some(found) = dotted_only_field(child) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(a) => a.iter().find_map(dotted_only_field),
        _ => None,
    }
}

/// `_id` and `_index` may arrive as strings or bare numbers.
fn scalar_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub async fn mget(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let default_index = index.map(|Path(i)| i);
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };

    let mut requested: Vec<(Option<String>, Option<String>, Option<Value>)> = Vec::new();
    let empty_docs = body
        .get("docs")
        .and_then(|d| d.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(false)
        || body.get("ids").and_then(|d| d.as_array()).map(|a| a.is_empty()).unwrap_or(false);
    if let Some(docs) = body.get("docs").and_then(|d| d.as_array()) {
        for d in docs {
            for dep in ["_routing", "_version", "_type", "routing", "version"] {
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
            requested.push((
                d.get("_index").and_then(scalar_str),
                d.get("_id").and_then(scalar_str),
                d.get("_source").cloned().or_else(|| {
                    d.get("stored_fields").map(|sf| json!({"__stored": sf}))
                }),
            ));
        }
    } else if let Some(ids) = body.get("ids").and_then(|d| d.as_array()) {
        for i in ids {
            requested.push((None, scalar_str(i), None));
        }
    }

    let mut docs = Vec::new();
    for (idx, id, sel) in requested {
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
            docs.push(json!({
                "_index": idx, "_id": id,
                "error": {
                    "type": "index_not_found_exception",
                    "reason": reason,
                    "index": idx, "resource.type": "index_expression", "resource.id": idx,
                    "index_uuid": "_na_",
                    "root_cause": [cause]
                }
            }));
            continue;
        };
        let g = st.read();
        match read_source(&g, &id) {
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
                    "_seq_no": 0, "_primary_term": 1, "found": true
                });
                let mut wants_source = true;
                if let Some(spec) = &stored_spec {
                    let mut sub = Params::new();
                    sub.insert("stored_fields".into(), spec.clone());
                    if let Some(f) = stored_fields(&src, &sub) {
                        d["fields"] = f;
                    }
                    wants_source = spec.split(',').any(|f| f.trim() == "_source")
                        || explicit_source.is_some();
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

// -------------------------------------------------------------------- update

pub async fn update_doc(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let patch: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    const UPDATE_KEYS: &[&str] = &[
        "doc", "upsert", "doc_as_upsert", "detect_noop", "_source", "script",
        "scripted_upsert", "if_seq_no", "if_primary_term",
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
    let existing = read_source(&g, &id);
    let detect_noop =
        patch.get("detect_noop").and_then(|v| v.as_bool()).unwrap_or(true);
    let doc_as_upsert = patch.get("doc_as_upsert").and_then(|v| v.as_bool()).unwrap_or(false);

    let (next, result) = match (existing.clone(), patch.get("doc")) {
        (Some(base), Some(d)) => {
            let mut merged = base.clone();
            merge_into(&mut merged, d);
            if detect_noop && merged == base {
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
    maybe_refresh(&mut g, &p);
    let status = if result == "created" { StatusCode::CREATED } else { StatusCode::OK };
    (status, axum::Json(body_out)).into_response()
}

// --------------------------------------------------------------- force merge

/// `_forcemerge` collapses segments. Fewer segments means less per-segment setup
/// on every search, which matters most for aggregations: each one opens columns
/// and builds its own intermediate result per segment before they are merged.
pub async fn force_merge(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_else(|| "_all".into());
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" {
        return no_such_index(&expr);
    }
    let max_segments: usize = p
        .get("max_num_segments")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .max(1);

    for name in targets {
        let Some(st) = store.get(&name) else { continue };
        let mut g = st.write();
        if g.refresh().is_err() {
            continue;
        }
        loop {
            let ids: Vec<tantivy::index::SegmentId> = g
                .index
                .searchable_segment_metas()
                .unwrap_or_default()
                .iter()
                .map(|m| m.id())
                .collect();
            if ids.len() <= max_segments {
                break;
            }
            // merge the whole set down in one step; tantivy handles the rest
            let take = ids.len() - max_segments + 1;
            let batch: Vec<_> = ids.into_iter().take(take).collect();
            let merged = match g.writer() {
                Ok(w) => w.merge(&batch).wait().is_ok(),
                Err(_) => false,
            };
            if !merged {
                break;
            }
            let _ = g.refresh();
        }
    }
    respond(&p, json!({"_shards": {"total": 1, "successful": 1, "failed": 0}}))
}

// --------------------------------------------------------------------- stats

fn index_stats(st: &IdxState) -> Value {
    let searcher = st.reader.searcher();
    let docs = searcher.num_docs();
    json!({
        "docs": {"count": docs, "deleted": 0},
        "store": {"size_in_bytes": 0, "reserved_in_bytes": 0},
        "indexing": {"index_total": docs, "index_time_in_millis": 0, "index_current": 0,
                     "index_failed": 0, "delete_total": 0, "delete_time_in_millis": 0,
                     "delete_current": 0, "noop_update_total": 0, "is_throttled": false,
                     "throttle_time_in_millis": 0},
        "get": {"total": 0, "time_in_millis": 0, "getTime": "0s", "exists_total": 0,
                "exists_time_in_millis": 0, "missing_total": 0,
                "missing_time_in_millis": 0, "current": 0},
        "search": {"open_contexts": 0, "query_total": st.search_count.load(std::sync::atomic::Ordering::Relaxed), "query_time_in_millis": 1,
                   "query_current": 0, "fetch_total": st.search_count.load(std::sync::atomic::Ordering::Relaxed), "fetch_time_in_millis": 1,
                   "fetch_current": 0, "scroll_total": 0, "scroll_time_in_millis": 0,
                   "scroll_current": 0, "suggest_total": 0, "suggest_time_in_millis": 0,
                   "suggest_current": 0},
        "merges": {"current": 0, "current_docs": 0, "current_size_in_bytes": 0,
                   "total": 0, "total_time_in_millis": 0, "total_docs": 0,
                   "total_size_in_bytes": 0},
        "refresh": {"total": 0, "total_time_in_millis": 0, "external_total": 0,
                    "external_total_time_in_millis": 0, "listeners": 0},
        "flush": {"total": 0, "periodic": 0, "total_time_in_millis": 0},
        "warmer": {"current": 0, "total": 0, "total_time_in_millis": 0},
        "query_cache": {"memory_size_in_bytes": 0, "total_count": 0, "hit_count": 0,
                        "miss_count": 0, "cache_size": 0, "cache_count": 0, "evictions": 0},
        "fielddata": {"memory_size_in_bytes": 0, "evictions": 0},
        "completion": {"size_in_bytes": 0},
        "segments": {"count": searcher.segment_readers().len(), "memory_in_bytes": 0,
                     "terms_memory_in_bytes": 0, "stored_fields_memory_in_bytes": 0,
                     "term_vectors_memory_in_bytes": 0, "norms_memory_in_bytes": 0,
                     "points_memory_in_bytes": 0, "doc_values_memory_in_bytes": 0,
                     "index_writer_memory_in_bytes": 0, "version_map_memory_in_bytes": 0,
                     "fixed_bit_set_memory_in_bytes": 0, "max_unsafe_auto_id_timestamp": -1,
                     "file_sizes": {}},
        "translog": {"operations": 0, "size_in_bytes": 0, "uncommitted_operations": 0,
                     "uncommitted_size_in_bytes": 0, "earliest_last_modified_age": 0,
                     "remote_store": {"upload": {"total_uploads": {"started": 0, "failed": 0, "succeeded": 0}}}},
        "request_cache": {
            "memory_size_in_bytes": 0, "evictions": 0, "hit_count": 0,
            "miss_count": st.request_cache_miss.load(std::sync::atomic::Ordering::Relaxed)
        },
        "recovery": {"current_as_source": 0, "current_as_target": 0, "throttle_time_in_millis": 0},
    })
}

fn sum_stats(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let mut out = x.clone();
            for (k, v) in y {
                let merged = match x.get(k) {
                    Some(prev) => sum_stats(prev, v),
                    None => v.clone(),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        (Value::Number(x), Value::Number(y)) => {
            json!(x.as_f64().unwrap_or(0.0) + y.as_f64().unwrap_or(0.0))
        }
        _ => b.clone(),
    }
}

/// `/_stats/{metric}` selects which sections to report; we always report all,
/// so the metric is only consumed to keep it off the index path.
pub async fn stats_metric(
    State(store): State<Store>,
    Path(_metric): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    stats_impl(store, "_all".into(), p)
}

pub async fn stats_index_metric(
    State(store): State<Store>,
    Path((index, _metric)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    stats_impl(store, index, p)
}

pub async fn stats(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    stats_impl(store, index.map(|Path(i)| i).unwrap_or_else(|| "_all".into()), p)
}

fn stats_impl(store: Store, expr: String, p: Params) -> Response {
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !ignore_unavailable(&p) {
        return no_such_index(&expr);
    }
    let level = p.get("level").map(|s| s.as_str()).unwrap_or("indices");
    let _ = &expr;
    let mut indices = serde_json::Map::new();
    let mut all = json!({});
    for n in &targets {
        let Some(st) = store.get(n) else { continue };
        let s = index_stats(&st.read());
        all = sum_stats(&all, &s);
        let mut entry = json!({
            "uuid": "_na_",
            "primaries": s.clone(),
            "total": s,
        });
        if level == "shards" {
            entry["shards"] = json!({"0": []});
        }
        indices.insert(n.clone(), entry);
    }
    let mut body = json!({
        "_shards": {"total": targets.len(), "successful": targets.len(), "failed": 0},
        "_all": {"primaries": all.clone(), "total": all},
    });
    if level != "cluster" {
        body["indices"] = Value::Object(indices);
    }
    axum::Json(body).into_response()
}

// ------------------------------------------------------------------- explain

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
    let q = body.get("query").cloned().unwrap_or(json!({"match_all": {}}));
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

// ---------------------------------------------------------------- field_caps

fn caps_for(kind: &str) -> Value {
    let aggregatable = kind != "text";
    let searchable = true;
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
            for kind in kinds {
            let entry = fields.entry(name.clone()).or_insert_with(|| json!({}));
            let slot = entry
                .as_object_mut()
                .unwrap()
                .entry(kind.clone())
                .or_insert_with(|| caps_for(&kind));
            if let Some(m) = meta.clone().and_then(|m| m.as_object().cloned()) {
                let dst = slot
                    .as_object_mut()
                    .unwrap()
                    .entry("meta".to_string())
                    .or_insert_with(|| json!({}));
                for (mk, mv) in m {
                    let list = dst
                        .as_object_mut()
                        .unwrap()
                        .entry(mk)
                        .or_insert_with(|| json!([]));
                    if let Some(a) = list.as_array_mut() {
                        if !a.contains(&mv) {
                            a.push(mv);
                        }
                    }
                }
            }
            // a type seen in only some indices lists the ones it came from
            let indices = slot
                .as_object_mut()
                .unwrap()
                .entry("__indices".to_string())
                .or_insert_with(|| json!([]));
            if let Some(a) = indices.as_array_mut() {
                a.push(json!(n));
            }
            }
        }
    }

    // only report `indices` on a field whose type is not uniform
    for (_, per_type) in fields.iter_mut() {
        let type_count = per_type.as_object().map(|o| o.len()).unwrap_or(0);
        if let Some(o) = per_type.as_object_mut() {
            for (_, v) in o.iter_mut() {
                let idx = v.as_object_mut().unwrap().remove("__indices");
                if type_count > 1 {
                    if let Some(i) = idx {
                        v["indices"] = i;
                    }
                }
            }
        }
    }

    respond(&p, json!({"indices": kept, "fields": Value::Object(fields)}))
}

// -------------------------------------------------------------------- alias

pub async fn get_alias(
    State(store): State<Store>,
    Query(p): Query<Params>,
) -> Response {
    let mut out = serde_json::Map::new();
    for n in store.names() {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        let mut aliases = serde_json::Map::new();
        for a in &g.aliases {
            aliases.insert(a.clone(), json!({}));
        }
        out.insert(n.clone(), json!({"aliases": Value::Object(aliases)}));
    }
    respond(&p, Value::Object(out))
}
