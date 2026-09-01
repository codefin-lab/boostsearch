//! Writing over the documents a query finds.
//!
//! `_delete_by_query`, `_update_by_query` and `_reindex` are the same walk:
//! run a query, take what it found, and write. They answer with the same
//! tally -- how many were looked at, how many were written, and what went
//! wrong -- so the tally is built once here.

use super::*;

/// One document, as the walk saw it.
pub(crate) struct Seen {
    index: String,
    id: String,
    source: Value,
    /// where the document stood when the walk read it
    seq_no: Option<u64>,
}

/// What a walk over a query's results did.
#[derive(Default)]
pub(crate) struct Tally {
    pub total: usize,
    pub created: usize,
    pub updated: usize,
    pub deleted: usize,
    pub noops: usize,
    pub version_conflicts: usize,
    pub failures: Vec<Value>,
}

impl Tally {
    /// A document that was written to since the walk read it.
    fn note_conflict(&mut self, seen: &Seen) {
        self.version_conflicts += 1;
        let id = &seen.id;
        let seq = seen.seq_no.unwrap_or(0);
        self.failures.push(json!({
            "index": seen.index, "id": id, "status": 409,
            "cause": {
                "type": "version_conflict_engine_exception",
                "reason": format!(
                    "[{id}]: version conflict, required seqNo [{seq}], primary term [1]"
                ),
                "index": seen.index, "shard": "0", "index_uuid": "_na_",
            },
        }));
    }

    /// The answer OpenSearch gives for a walk of this kind.
    fn answer(&self, took: u128, batches: usize) -> Value {
        json!({
            "took": took as u64,
            "timed_out": false,
            "total": self.total,
            "updated": self.updated,
            "created": self.created,
            "deleted": self.deleted,
            "batches": batches,
            "version_conflicts": self.version_conflicts,
            "noops": self.noops,
            "retries": {"bulk": 0, "search": 0},
            "throttled_millis": 0,
            "requests_per_second": -1.0,
            "throttled_until_millis": 0,
            "failures": self.failures,
        })
    }
}

/// Every document a query finds, as `(index, id, source)`.
///
/// The walk reads them all before it writes any: writing while the reader is
/// still open would have it read what the walk itself had just written.
fn found(
    store: &Store,
    expr: &str,
    body: &Value,
    p: &Params,
) -> std::result::Result<Vec<Seen>, Response> {
    let limit = p
        .get("max_docs")
        .or_else(|| p.get("size"))
        .and_then(|v| v.parse::<usize>().ok())
        .or_else(|| body.get("max_docs").and_then(|v| v.as_u64()).map(|v| v as usize))
        .unwrap_or(10_000);
    let query = body.get("query").cloned().unwrap_or_else(|| json!({"match_all": {}}));
    // the sequence number each document stood at is what makes a write
    // conditional: one written since is a conflict, not a document to write
    let mut request =
        json!({"query": query, "size": limit, "_source": true, "seq_no_primary_term": true});
    if let Some(sort) = body.get("sort") {
        request["sort"] = sort.clone();
    }
    let answer = crate::search::run(store, expr, &request, &Params::new())?;
    Ok(answer
        .hits
        .into_iter()
        .filter_map(|hit| {
            Some(Seen {
                index: hit.get("_index")?.as_str()?.to_string(),
                id: hit.get("_id")?.as_str()?.to_string(),
                source: hit.get("_source").cloned().unwrap_or_else(|| json!({})),
                seq_no: hit.get("_seq_no").and_then(|v| v.as_u64()),
            })
        })
        .collect())
}

/// What the request gets wrong before any index is looked at.
fn complaint(p: &Params, body: &Value) -> Option<Response> {
    // the body may say it too, and the two have to agree
    if let (Some(named), Some(asked)) =
        (p.get("max_docs").or_else(|| p.get("size")), body.get("max_docs").and_then(|v| v.as_i64()))
        && named.parse::<i64>().map(|n| n != asked).unwrap_or(false)
    {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("[max_docs] set to two different values [{named}] and [{asked}]"),
        ));
    }
    if let Some(asked) = body.get("max_docs").and_then(|v| v.as_i64())
        && asked < 0
    {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("[max_docs] parameter cannot be negative, found [{asked}]"),
        ));
    }
    if let Some(rate) = p.get("requests_per_second") {
        let asked = rate.parse::<f64>().ok();
        let allowed = matches!(asked, Some(r) if r > 0.0 || r == -1.0);
        if !allowed {
            return Some(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                "[requests_per_second] must be a float greater than 0. Use -1 to disable \
                 throttling.",
            ));
        }
    }
    if let Some(slices) = p.get("slices")
        && slices != "auto"
        && slices.parse::<u64>().map(|n| n == 0).unwrap_or(true)
    {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "[slices] must be a positive integer or the string \"auto\"",
        ));
    }
    if let (Some(a), Some(b)) = (p.get("max_docs"), p.get("size"))
        && a != b
    {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("[max_docs] set to two different values [{b}] and [{a}]"),
        ));
    }
    for named in ["max_docs", "size"] {
        if let Some(asked) = p.get(named)
            && asked.parse::<i64>().map(|n| n < 0).unwrap_or(false)
        {
            return Some(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("[max_docs] parameter cannot be negative, found [{asked}]"),
            ));
        }
    }
    if let (Some(docs), Some(slices)) = (
        p.get("max_docs").or_else(|| p.get("size")).and_then(|v| v.parse::<u64>().ok()),
        p.get("slices").and_then(|v| v.parse::<u64>().ok()),
    ) && docs < slices
    {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "[max_docs] should be >= [slices]",
        ));
    }
    if let Some(size) = p.get("scroll_size")
        && size.parse::<i64>().map(|n| n < 0).unwrap_or(true)
    {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("Failed to parse int parameter [scroll_size] with value [{size}]"),
        ));
    }
    None
}

/// Whether the request asked for the walk to be done in the background.
///
/// Nothing here takes long enough to need it, so the walk is done and the
/// task it would have been is reported as finished.
fn as_task(p: &Params) -> bool {
    p.get("wait_for_completion").map(|v| v == "false").unwrap_or(false)
}

/// The name a finished task is reported under.
fn task_name(store: &Store) -> String {
    let seq = store.next_task_id();
    format!("{}:{}", crate::store::index_uuid("node"), seq)
}

pub async fn delete_by_query(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let started = std::time::Instant::now();
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    if let Some(complaint) = complaint(&p, &body) {
        return complaint;
    }
    let hits = match found(&store, &index, &body, &p) {
        Ok(hits) => hits,
        Err(e) => return e,
    };
    let mut tally = Tally { total: hits.len(), ..Default::default() };
    let proceed = body.get("conflicts").and_then(|v| v.as_str()) == Some("proceed")
        || p.get("conflicts").map(|v| v == "proceed").unwrap_or(false);
    for seen in hits {
        let Some(st) = store.get(&seen.index) else { continue };
        let mut g = st.write();
        if moved_on(&g, &seen) {
            tally.version_conflicts += 1;
            if !proceed {
                tally.note_conflict(&seen);
                break;
            }
            continue;
        }
        let (_, status) = delete_doc(&mut g, &seen.id);
        if status == StatusCode::OK {
            tally.deleted += 1;
        } else {
            tally.noops += 1;
        }
    }
    for name in store.resolve(&index) {
        if let Some(st) = store.get(&name) {
            let _ = st.write().refresh();
        }
    }
    finish(&store, tally, started, &p)
}

pub async fn update_by_query(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let started = std::time::Instant::now();
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    if let Some(complaint) = complaint(&p, &body) {
        return complaint;
    }
    // a script would say what to change; without one the walk rewrites each
    // document as it stands, which is what gives it a new version
    if body.get("script").is_some() {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "scripts are not supported yet",
        );
    }
    let hits = match found(&store, &index, &body, &p) {
        Ok(hits) => hits,
        Err(e) => return e,
    };
    let mut tally = Tally { total: hits.len(), ..Default::default() };
    let proceed = body.get("conflicts").and_then(|v| v.as_str()) == Some("proceed")
        || p.get("conflicts").map(|v| v == "proceed").unwrap_or(false);
    for seen in hits {
        let Some(st) = store.get(&seen.index) else { continue };
        let mut g = st.write();
        if moved_on(&g, &seen) {
            tally.version_conflicts += 1;
            if !proceed {
                tally.note_conflict(&seen);
                break;
            }
            continue;
        }
        match write_doc_raw(&mut g, &seen.id, seen.source.clone(), "index", None) {
            Ok(_) => tally.updated += 1,
            Err(_) => {
                tally.version_conflicts += 1;
                if !proceed {
                    tally.note_conflict(&seen);
                    break;
                }
            }
        }
    }
    for name in store.resolve(&index) {
        if let Some(st) = store.get(&name) {
            let _ = st.write().refresh();
        }
    }
    finish(&store, tally, started, &p)
}

pub async fn reindex(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let started = std::time::Instant::now();
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    if let Some(complaint) = complaint(&p, &body) {
        return complaint;
    }
    if body.get("script").is_some() {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "scripts are not supported yet",
        );
    }
    let source = body.get("source").cloned().unwrap_or_else(|| json!({}));
    let dest = body.get("dest").cloned().unwrap_or_else(|| json!({}));
    if source.get("remote").is_some() {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "reindex from a remote cluster is not supported yet",
        );
    }
    let Some(from) = index_name(source.get("index")) else {
        return err(StatusCode::BAD_REQUEST, "action_request_validation_exception", "source index");
    };
    let Some(to) = dest.get("index").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: index is missing;",
        );
    };
    if store.resolve(&from).contains(&to) {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            format!(
                "Validation Failed: 1: reindex cannot write into an index its reading from [{to}];"
            ),
        );
    }
    let hits = match found(&store, &from, &source, &p) {
        Ok(hits) => hits,
        Err(e) => return e,
    };
    // a document may be written only where it is not already, if asked
    let create_only = dest.get("op_type").and_then(|v| v.as_str()) == Some("create");
    let conflicts_proceed = body.get("conflicts").and_then(|v| v.as_str()) == Some("proceed");
    let kept = source.get("_source").cloned();
    if store.ensure(&to).is_err() {
        return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", "cannot open dest");
    }
    let mut tally = Tally { total: hits.len(), ..Default::default() };
    for seen in hits {
        let mut document = seen.source.clone();
        if let Some(fields) = kept.as_ref() {
            document = only_these(&document, fields);
        }
        let Some(st) = store.get(&to) else { continue };
        let mut g = st.write();
        let existed = crate::api::doc::exists_doc(&g, &seen.id);
        let op = if create_only { "create" } else { "index" };
        match write_doc_raw(&mut g, &seen.id, document, op, None) {
            Ok(_) if existed => tally.updated += 1,
            Ok(_) => tally.created += 1,
            Err(_) => {
                tally.version_conflicts += 1;
                if !conflicts_proceed {
                    tally.failures.push(json!({
                        "index": to, "id": seen.id, "status": 409,
                        "cause": {
                            "type": "version_conflict_engine_exception",
                            "reason": format!(
                                "[{}]: version conflict, document already exists (solved by \
                                 specifying op_type=index)",
                                seen.id
                            ),
                            "index": to, "shard": "0", "index_uuid": "_na_",
                        },
                    }));
                    break;
                }
            }
        }
    }
    if let Some(st) = store.get(&to) {
        let _ = st.write().refresh();
    }
    finish(&store, tally, started, &p)
}

/// Whether the document has been written to since the walk read it.
fn moved_on(g: &IdxState, seen: &Seen) -> bool {
    match (seen.seq_no, read_seq(g, &seen.id)) {
        (Some(saw), Some(now)) => saw != now,
        _ => false,
    }
}

/// The source index a request names, which may be written as a list.
fn index_name(named: Option<&Value>) -> Option<String> {
    match named? {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => {
            let names: Vec<String> =
                a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
            (!names.is_empty()).then(|| names.join(","))
        }
        _ => None,
    }
}

/// A document with only the fields the request asked to carry over.
fn only_these(document: &Value, fields: &Value) -> Value {
    let wanted: Vec<String> = match fields {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
        _ => return document.clone(),
    };
    let mut out = serde_json::Map::new();
    for field in wanted {
        if let Some(value) = document.get(&field) {
            out.insert(field, value.clone());
        }
    }
    Value::Object(out)
}

/// The answer, or the name of the task it would have been.
fn finish(store: &Store, tally: Tally, started: std::time::Instant, p: &Params) -> Response {
    // the walk reads a page at a time, and says how many pages it read
    let per_batch = p
        .get("scroll_size")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(1000);
    let batches = tally.total.div_ceil(per_batch).max(1);
    let mut answer = tally.answer(started.elapsed().as_millis(), batches);
    // a walk asked to be sliced is one walk here, and says so slice by slice
    if let Some(slices) = p.get("slices").and_then(|v| v.parse::<usize>().ok()).filter(|n| *n > 1) {
        let each: Vec<Value> = (0..slices)
            .map(|slice| {
                json!({
                    "slice_id": slice, "total": 0, "updated": 0, "created": 0, "deleted": 0,
                    "batches": 0, "version_conflicts": 0, "noops": 0,
                    "retries": {"bulk": 0, "search": 0}, "throttled_millis": 0,
                    "requests_per_second": -1.0, "throttled_until_millis": 0, "failures": [],
                })
            })
            .collect();
        answer["slices"] = json!(each);
    }
    if as_task(p) {
        let name = task_name(store);
        store.remember_task(&name, answer);
        return axum::Json(json!({ "task": name })).into_response();
    }
    // a walk that could not write what it found says so in its status
    let status = if tally.failures.is_empty() { StatusCode::OK } else { StatusCode::CONFLICT };
    (status, axum::Json(answer)).into_response()
}

/// `_rethrottle` -- nothing here is throttled, so there is nothing to change.
pub async fn rethrottle(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let answer = store.task_answer(&id).unwrap_or_else(|| json!({}));
    respond(
        &p,
        json!({
            "nodes": {
                "node-0": {
                    "name": "boostsearch", "transport_address": "127.0.0.1:9300",
                    "host": "127.0.0.1", "ip": "127.0.0.1:9300", "roles": ["data"],
                    "tasks": {
                        id.clone(): {
                            "node": "node-0", "id": 1, "type": "transport",
                            "action": "indices:data/write/by_query",
                            "status": answer,
                            "description": id,
                            "start_time_in_millis": 0, "running_time_in_nanos": 0,
                            "cancellable": true,
                        }
                    },
                }
            },
            "node_failures": [],
        }),
    )
}
