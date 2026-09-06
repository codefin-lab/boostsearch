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
    limit: usize,
) -> std::result::Result<Vec<Seen>, Response> {
    let query = body.get("query").cloned().unwrap_or_else(|| json!({"match_all": {}}));
    // the sequence number each document stood at is what makes a write
    // conditional: one written since is a conflict, not a document to write
    let mut request =
        json!({"query": query, "size": limit, "_source": true, "seq_no_primary_term": true});
    if let Some(sort) = body.get("sort") {
        request["sort"] = sort.clone();
    }
    let answer = crate::search::run(store, expr, &request, &Params::new())?;
    // a walk rewrites what it reads, so a document whose source was never
    // stored is one it cannot carry over
    for hit in &answer.hits {
        if hit.get("_source").is_none() {
            let index = hit.get("_index").and_then(|v| v.as_str()).unwrap_or_default();
            let id = hit.get("_id").and_then(|v| v.as_str()).unwrap_or_default();
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("[{index}][{id}] didn't store _source"),
            ));
        }
    }
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

/// How many documents the walk may write.
///
/// The request may say it in the URL or in the body, and the body may spell
/// it `size`, which is the older name for the same thing.
fn max_docs(p: &Params, body: &Value) -> usize {
    p.get("max_docs")
        .or_else(|| p.get("size"))
        .and_then(|v| v.parse::<usize>().ok())
        .or_else(|| {
            body.get("max_docs")
                .or_else(|| body.get("size"))
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
        })
        .unwrap_or(10_000)
}

/// How many documents the walk reads before it writes them.
///
/// `_reindex` names it inside the search it reads with; the other two name it
/// in the URL. It decides how many batches the walk reports, and how long a
/// throttled walk waits between them.
fn batch_size(p: &Params, source: &Value) -> usize {
    p.get("scroll_size")
        .and_then(|v| v.parse::<usize>().ok())
        .or_else(|| source.get("size").and_then(|v| v.as_u64()).map(|v| v as usize))
        .filter(|n| *n > 0)
        .unwrap_or(1000)
}

/// What the request gets wrong before any index is looked at.
fn complaint(p: &Params, body: &Value) -> Option<Response> {
    if let Some(complaint) = conflicts_complaint(
        p.get("conflicts")
            .map(|v| v.to_string())
            .or_else(|| body.get("conflicts").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .as_deref(),
    ) {
        return Some(complaint);
    }
    // the body may say it in two spellings, and they have to agree
    if let (Some(a), Some(b)) =
        (body.get("size").and_then(|v| v.as_i64()), body.get("max_docs").and_then(|v| v.as_i64()))
        && a != b
    {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("[max_docs] set to two different values [{a}] and [{b}]"),
        ));
    }
    if let Some(asked) = body.get("size").and_then(|v| v.as_i64())
        && asked < 0
    {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("[max_docs] parameter cannot be negative, found [{asked}]"),
        ));
    }
    // the body may say it too, and the two have to agree
    if let (Some(named), Some(asked)) = (
        p.get("max_docs").or_else(|| p.get("size")),
        body.get("max_docs").or_else(|| body.get("size")).and_then(|v| v.as_i64()),
    ) && named.parse::<i64>().map(|n| n != asked).unwrap_or(false)
    {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("[max_docs] set to two different values [{asked}] and [{named}]"),
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

/// What the search half of the request gets wrong.
///
/// A walk reads with a search, but not every search option means anything
/// when the reader is going to write what it finds: paging past the first
/// page would skip documents, and a walk needs the whole document, not the
/// fields a search would carry back.
fn search_complaint(source: &Value) -> Option<Response> {
    let refused = [
        ("from", "from is not supported in this context"),
        ("stored_fields", "stored_fields is not supported in this context"),
    ];
    for (named, why) in refused {
        if source.get(named).is_some() {
            return Some(err(StatusCode::BAD_REQUEST, "illegal_argument_exception", why));
        }
    }
    if source.get("_source").and_then(|v| v.as_bool()) == Some(false) {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "_source:false is not supported in this context",
        ));
    }
    // `size` names how many documents to walk, so it has to be a number
    if let Some(size) = source.get("size")
        && !size.is_number()
    {
        let written = size.as_str().map(|s| s.to_string()).unwrap_or_else(|| size.to_string());
        return Some(err(
            StatusCode::BAD_REQUEST,
            "number_format_exception",
            format!("For input string: \"{written}\""),
        ));
    }
    None
}

/// How a walk was told to deal with a document written since it was read.
fn conflicts_complaint(named: Option<&str>) -> Option<Response> {
    match named {
        None | Some("proceed") | Some("abort") => None,
        Some(other) => Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("conflicts may only be \"proceed\" or \"abort\" but was [{other}]"),
        )),
    }
}

/// What a `_reindex` body gets wrong before any index is looked at.
///
/// The order the complaints come in is the order OpenSearch reads the body:
/// what it cannot parse at all, then the destination, then how the walk was
/// asked to run, then the search it reads with.
fn reindex_complaint(body: &Value) -> Option<Response> {
    const BODY_FIELDS: &[&str] =
        &["source", "dest", "conflicts", "size", "max_docs", "script", "slices"];
    const DEST_FIELDS: &[&str] =
        &["index", "op_type", "routing", "pipeline", "version_type", "type"];
    const SOURCE_FIELDS: &[&str] = &[
        "index",
        "query",
        "sort",
        "size",
        "from",
        "_source",
        "stored_fields",
        "remote",
        "slice",
        "search_after",
        "type",
        "scroll_size",
        "runtime_mappings",
    ];
    let named = |field: &str| {
        err(
            StatusCode::BAD_REQUEST,
            "x_content_parse_exception",
            format!("[reindex] unknown field [{field}]"),
        )
    };
    for key in body.as_object().into_iter().flatten().map(|(k, _)| k) {
        if !BODY_FIELDS.contains(&key.as_str()) {
            return Some(named(key));
        }
    }
    if let Some(dest) = body.get("dest").and_then(|v| v.as_object()) {
        for key in dest.keys() {
            if !DEST_FIELDS.contains(&key.as_str()) {
                return Some(err(
                    StatusCode::BAD_REQUEST,
                    "x_content_parse_exception",
                    format!("[dest] unknown field [{key}]"),
                ));
            }
        }
    }
    if let Some(source) = body.get("source").and_then(|v| v.as_object()) {
        for (key, value) in source {
            if !SOURCE_FIELDS.contains(&key.as_str()) {
                let start = match value.is_object() {
                    true => format!("Unknown key for a START_OBJECT in [{key}]."),
                    false => format!("Unknown key for a VALUE_STRING in [{key}]."),
                };
                return Some(err(StatusCode::BAD_REQUEST, "parsing_exception", start));
            }
        }
    }
    if let Some(complaint) = conflicts_complaint(body.get("conflicts").and_then(|v| v.as_str())) {
        return Some(complaint);
    }
    if body.get("dest").is_some_and(|d| d.get("index").is_none()) {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: index must be specified;",
        ));
    }
    if let Some(source) = body.get("source")
        && let Some(complaint) = search_complaint(source)
    {
        return Some(complaint);
    }
    if let Some(remote) = body.pointer("/source/remote")
        && let Some(complaint) = remote_complaint(remote)
    {
        return Some(complaint);
    }
    None
}

/// What a `remote` block gets wrong.
///
/// Nothing here reads from another cluster yet, but a body that could never
/// name one is refused for the reason it could not, which is what a caller
/// has to fix first either way.
fn remote_complaint(remote: &Value) -> Option<Response> {
    const REMOTE_FIELDS: &[&str] =
        &["host", "username", "password", "headers", "socket_timeout", "connect_timeout"];
    let host = remote.get("host").and_then(|v| v.as_str()).unwrap_or_default();
    let shaped = host.starts_with("http://") || host.starts_with("https://");
    if !shaped {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "[host] must be of the form [scheme]://[host]:[port]",
        ));
    }
    let unknown: Vec<&str> = remote
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, _)| k.as_str())
        .filter(|k| !REMOTE_FIELDS.contains(k))
        .collect();
    if !unknown.is_empty() {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("Unsupported fields in [remote]. [{}]", unknown.join(",")),
        ));
    }
    for named in ["socket_timeout", "connect_timeout"] {
        if let Some(written) = remote.get(named).and_then(|v| v.as_str())
            && crate::search::extras::parse_time_amount(written).is_none()
        {
            return Some(err(
                StatusCode::BAD_REQUEST,
                "number_format_exception",
                format!("failed to parse setting [{named}] with value [{written}] as a time value"),
            ));
        }
    }
    // a host that could be read is still not one this node was told it may
    // read from
    let named = named_host(host);
    if remote_allowed(&named) {
        return None;
    }
    Some(err(
        StatusCode::BAD_REQUEST,
        "illegal_argument_exception",
        format!("[{named}] not allowlisted in reindex.remote.allowlist"),
    ))
}

/// A remote host as the allowlist spells it: the authority, without a scheme
/// and without a path.
fn named_host(host: &str) -> String {
    host.trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Whether this node was told it may read from a host.
///
/// `reindex.remote.allowlist` is a node setting rather than a cluster one --
/// reading from another cluster is a thing an operator allows, not something
/// a client may allow itself -- so it is read from the node's configuration,
/// as `host:port` entries where either half may be `*`. Nothing is allowed
/// unless it is named, which is why a node with no setting refuses every
/// remote.
fn remote_allowed(named: &str) -> bool {
    let Ok(listed) = std::env::var("BOOSTSEARCH_REINDEX_ALLOWLIST") else {
        return false;
    };
    let (host, port) = named.rsplit_once(':').unwrap_or((named, ""));
    listed.split(',').map(str::trim).filter(|s| !s.is_empty()).any(|entry| {
        let (allowed_host, allowed_port) = entry.rsplit_once(':').unwrap_or((entry, "*"));
        let host_ok = allowed_host == "*" || allowed_host == host;
        let port_ok = allowed_port == "*" || allowed_port == port;
        host_ok && port_ok
    })
}

/// The documents a remote cluster holds for a query.
///
/// A remote is read the way any client reads it: a search over HTTP, then
/// scrolls until it stops giving anything back, and the scroll closed
/// afterwards so the other cluster is not left holding a context. What comes
/// back is the same `Seen` a local read produces, so everything downstream --
/// the script, the destination, the tally -- cannot tell the difference.
fn found_remote(
    remote: &Value,
    expr: &str,
    source: &Value,
    limit: usize,
    batch: usize,
) -> std::result::Result<Vec<Seen>, Response> {
    let host = remote.get("host").and_then(|v| v.as_str()).unwrap_or_default().trim_end_matches('/');
    let query = source.get("query").cloned().unwrap_or_else(|| json!({"match_all": {}}));
    let mut request = json!({"query": query, "size": batch.min(limit.max(1))});
    if let Some(kept) = source.get("_source") {
        request["_source"] = kept.clone();
    }
    let timeout = remote
        .get("socket_timeout")
        .and_then(|v| v.as_str())
        .and_then(crate::search::extras::parse_time_amount)
        .unwrap_or(30_000.0);
    let call = |url: String, body: Value| -> std::result::Result<Value, Response> {
        // a refusal is an answer with a body, and the body says what was
        // wrong with the request -- which is what the caller asked for, so it
        // must not be turned into an error that throws the body away
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_millis(timeout.max(1.0) as u64)))
            .http_status_as_error(false)
            .build()
            .into();
        let mut request = agent.post(&url).header("content-type", "application/json");
        if let (Some(user), Some(password)) = (
            remote.get("username").and_then(|v| v.as_str()),
            remote.get("password").and_then(|v| v.as_str()),
        ) {
            use base64::Engine;
            let encoded =
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
            request = request.header("authorization", format!("Basic {encoded}"));
        }
        for (name, value) in remote.get("headers").and_then(|v| v.as_object()).into_iter().flatten()
        {
            if let Some(text) = value.as_str() {
                request = request.header(name, text);
            }
        }
        match request.send_json(&body) {
            Ok(mut answer) => {
                answer.body_mut().read_json::<Value>().map_err(|e| remote_failure(format!("{e}")))
            }
            Err(e) => Err(remote_failure(format!("{e}"))),
        }
    };
    let first = call(format!("{host}/{expr}/_search?scroll=5m"), request)?;
    if let Some(reason) = first.pointer("/error/reason").and_then(|v| v.as_str()) {
        return Err(remote_failure(reason.to_string()));
    }
    let mut out = Vec::new();
    let mut scroll = first.get("_scroll_id").and_then(|v| v.as_str()).map(|s| s.to_string());
    let mut page = first;
    loop {
        let hits: Vec<Value> = page
            .pointer("/hits/hits")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if hits.is_empty() {
            break;
        }
        for hit in hits {
            if out.len() >= limit {
                break;
            }
            let Some(id) = hit.get("_id").and_then(|v| v.as_str()) else {
                continue;
            };
            out.push(Seen {
                index: hit.get("_index").and_then(|v| v.as_str()).unwrap_or(expr).to_string(),
                id: id.to_string(),
                source: hit.get("_source").cloned().unwrap_or_else(|| json!({})),
                // a document read from another cluster stands at no sequence
                // number here, so a write of it is not conditional on one
                seq_no: None,
            });
        }
        if out.len() >= limit {
            break;
        }
        let Some(held) = scroll.clone() else { break };
        page = call(format!("{host}/_search/scroll"), json!({"scroll": "5m", "scroll_id": held}))?;
        scroll = page.get("_scroll_id").and_then(|v| v.as_str()).map(|s| s.to_string());
    }
    // the other cluster should not be left holding a context this walk is done
    // with, whether or not it minds
    if let Some(held) = scroll {
        let _ = ureq::delete(&format!("{host}/_search/scroll?scroll_id={held}")).call();
    }
    Ok(out)
}

/// A remote that could not be read, in the words a client expects.
fn remote_failure(reason: String) -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, "connect_exception", reason)
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
    if let Some(complaint) = complaint(&p, &body).or_else(|| search_complaint(&body)) {
        return complaint;
    }
    // a walk that deletes has to be told what to delete
    if body.get("query").is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: query is missing;",
        );
    }
    let hits = match found(&store, &index, &body, max_docs(&p, &body)) {
        Ok(hits) => hits,
        Err(e) => return e,
    };
    if let Some(failure) = too_few_copies(&store, &index, &p) {
        let tally = Tally { total: hits.len(), failures: vec![failure], ..Default::default() };
        return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(tally.answer(0, 1))).into_response();
    }
    let mut tally = Tally { total: hits.len(), ..Default::default() };
    let proceed = body.get("conflicts").and_then(|v| v.as_str()) == Some("proceed")
        || p.get("conflicts").map(|v| v == "proceed").unwrap_or(false);
    // `?pipeline=` names one every rewritten document goes through
    let through = p.get("pipeline").cloned();
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
    finish(&store, tally, started, &p, &index, batch_size(&p, &body)).await
}

pub async fn update_by_query(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let started = std::time::Instant::now();
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    if let Some(complaint) = complaint(&p, &body).or_else(|| search_complaint(&body)) {
        return complaint;
    }
    // a script says what to change; without one the walk rewrites each
    // document as it stands, which is what gives it a new version
    let script = match body.get("script") {
        Some(spec) => {
            match crate::painless::contexts::Compiled::of(spec, &|id| store.stored_script(id)) {
                Ok(c) => Some(c),
                Err(e) if e.kind == "compile error" => return crate::api::compile_failure(e),
                Err(e) => return crate::api::script_failure(e),
            }
        }
        None => None,
    };
    let hits = match found(&store, &index, &body, max_docs(&p, &body)) {
        Ok(hits) => hits,
        Err(e) => return e,
    };
    if let Some(failure) = too_few_copies(&store, &index, &p) {
        let tally = Tally { total: hits.len(), failures: vec![failure], ..Default::default() };
        return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(tally.answer(0, 1))).into_response();
    }
    let mut tally = Tally { total: hits.len(), ..Default::default() };
    let proceed = body.get("conflicts").and_then(|v| v.as_str()) == Some("proceed")
        || p.get("conflicts").map(|v| v == "proceed").unwrap_or(false);
    // `?pipeline=` names one every rewritten document goes through
    let through = p.get("pipeline").cloned();
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
        // the script sees the document in `ctx` and may change it, leave
        // it, or have it deleted
        let mut next = seen.source.clone();
        let mut op = "index";
        if let Some(compiled) = &script {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let ctx = crate::painless::contexts::update_ctx(
                &g.name,
                &seen.id,
                g.version_of(&seen.id),
                &seen.source,
                now,
                "index",
            );
            let mut runner =
                crate::painless::contexts::Runner::new(&compiled.params).with_ctx(ctx.clone());
            if let Err(e) = runner.run(&compiled.script) {
                return crate::search::search_script_failure_partial(e, &seen.index);
            }
            match scripted_change(&ctx, &seen.id) {
                Ok((changed_op, source)) => {
                    op = changed_op;
                    next = source;
                }
                Err(reason) => {
                    return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", reason);
                }
            }
        }
        match op {
            "noop" => {
                tally.noops += 1;
                continue;
            }
            "delete" => {
                let _ = crate::api::doc::delete_doc(&mut g, &seen.id);
                tally.deleted += 1;
                continue;
            }
            _ => {}
        }
        // a document rewritten in place is written the way any document is,
        // so a pipeline the request named runs over it. The index is held for
        // writing here, and a pipeline may read the store, so it runs with
        // the lock let go and taken again.
        if let Some(named) = &through {
            drop(g);
            let piped = crate::api::ingest::ingest_for_write(
                &store,
                &seen.index,
                &seen.id,
                next,
                Some(named),
                None,
            );
            g = st.write();
            match piped {
                Ok(Some(doc)) => next = doc.source,
                Ok(None) => {
                    tally.noops += 1;
                    continue;
                }
                Err(e) => {
                    tally.failures.push(json!({
                        "index": seen.index, "id": seen.id, "status": 400,
                        "cause": {"type": e.kind, "reason": e.reason},
                    }));
                    continue;
                }
            }
        }
        match write_doc_raw(&mut g, &seen.id, next, "index", None) {
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
    drop(script);
    for name in store.resolve(&index) {
        if let Some(st) = store.get(&name) {
            let _ = st.write().refresh();
        }
    }
    finish(&store, tally, started, &p, &index, batch_size(&p, &body)).await
}

pub async fn reindex(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let started = std::time::Instant::now();
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    if let Some(complaint) = reindex_complaint(&body) {
        return complaint;
    }
    if let Some(complaint) = complaint(&p, &body) {
        return complaint;
    }
    let script = match body.get("script") {
        Some(spec) => {
            match crate::painless::contexts::Compiled::of(spec, &|id| store.stored_script(id)) {
                Ok(c) => Some(c),
                Err(e) if e.kind == "compile error" => return crate::api::compile_failure(e),
                Err(e) => return crate::api::script_failure(e),
            }
        }
        None => None,
    };
    let source = body.get("source").cloned().unwrap_or_else(|| json!({}));
    let dest = body.get("dest").cloned().unwrap_or_else(|| json!({}));
    let remote = source.get("remote").cloned();
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
    // reading from another cluster, an index of the same name is a different
    // index, so writing into it is not writing into what is being read
    if remote.is_none() && store.resolve(&from).contains(&to) {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            format!(
                "Validation Failed: 1: reindex cannot write into an index its reading from [{to}];"
            ),
        );
    }
    let wanted = max_docs(&p, &body);
    let hits = match &remote {
        // reading another cluster is waiting on a socket, and waiting on a
        // socket from inside a request handler is how a node stops answering:
        // the wait holds a worker, and what it is waiting for may be this
        // node itself. So it happens off the runtime.
        Some(remote) => {
            let (remote, from, source) = (remote.clone(), from.clone(), source.clone());
            let batch = batch_size(&p, &source);
            let read = tokio::task::spawn_blocking(move || {
                found_remote(&remote, &from, &source, wanted, batch)
            })
            .await;
            match read {
                Ok(Ok(hits)) => hits,
                Ok(Err(e)) => return e,
                Err(e) => return remote_failure(format!("{e}")),
            }
        }
        None => match found(&store, &from, &source, wanted) {
            Ok(hits) => hits,
            Err(e) => return e,
        },
    };
    // a document may be written only where it is not already, if asked
    let create_only = dest.get("op_type").and_then(|v| v.as_str()) == Some("create");
    let conflicts_proceed = body.get("conflicts").and_then(|v| v.as_str()) == Some("proceed");
    let kept = source.get("_source").cloned();
    // `dest.pipeline` names a pipeline every document goes through on the way
    // in, the same one an index request would name in its URL
    let through = dest.get("pipeline").and_then(|v| v.as_str()).map(|s| s.to_string());
    // a destination that is not there yet is created, unless the cluster was
    // told which names may be created on the fly
    if store.get(&to).is_none()
        && let Some(complaint) = auto_create_complaint(&store, &to)
    {
        return complaint;
    }
    // with a script, the destination is made only when a document is
    // written to it: the script may send them all elsewhere, or drop them
    if script.is_none() && store.ensure(&to).is_err() {
        return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", "cannot open dest");
    }
    if let Some(failure) = too_few_copies(&store, &to, &p) {
        let tally = Tally { total: hits.len(), failures: vec![failure], ..Default::default() };
        return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(tally.answer(0, 1))).into_response();
    }
    let mut tally = Tally { total: hits.len(), ..Default::default() };
    // every index a document was written to is refreshed at the end; a
    // script may have sent some elsewhere
    let mut written: Vec<String> = vec![to.clone()];
    // where a destination names a routing, it decides which shard each
    // document lands on: `=value` writes them all under one, `discard` drops
    // the one the source carried, and `keep` leaves it as it stands
    let routing = dest.get("routing").and_then(|v| v.as_str()).map(|s| s.to_string());
    for seen in hits {
        let mut document = seen.source.clone();
        if let Some(fields) = kept.as_ref() {
            document = only_these(&document, fields);
        }
        // the script may send the document elsewhere, rename it, route it,
        // or say it is not to be written at all
        let mut to = to.clone();
        let mut id = seen.id.clone();
        let mut scripted_routing: Option<String> = None;
        let mut op_asked = "index";
        if let Some(compiled) = &script {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let ctx = crate::painless::contexts::update_ctx(&to, &id, 1, &document, now, "index");
            let mut runner =
                crate::painless::contexts::Runner::new(&compiled.params).with_ctx(ctx.clone());
            if let Err(e) = runner.run(&compiled.script) {
                return crate::search::search_script_failure_partial(e, &seen.index);
            }
            let extra = crate::painless::contexts::ctx_extra_keys(&ctx);
            if let Some(junk) = extra.first() {
                return err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!("Invalid fields added to context [{junk}]"),
                );
            }
            let (op, src, changed_id, changed_routing) =
                match crate::painless::contexts::read_ctx(&ctx) {
                    Ok(read) => read,
                    Err(reason) => {
                        return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", reason);
                    }
                };
            if let crate::painless::Value::Map(m) = &ctx
                && let Some(index_now) =
                    crate::painless::value::map_get(m, &crate::painless::Value::str("_index"))
            {
                to = index_now.as_text();
            }
            match op.as_str() {
                "noop" => {
                    tally.noops += 1;
                    continue;
                }
                "delete" => {
                    // the document deleted is the one the script named
                    let target = changed_id.clone().unwrap_or_else(|| id.clone());
                    if let Some(st) = store.get(&to) {
                        let _ = crate::api::doc::delete_doc(&mut st.write(), &target);
                    }
                    tally.deleted += 1;
                    continue;
                }
                "index" | "create" => op_asked = if op == "create" { "create" } else { "index" },
                other => {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        format!(
                            "Operation type [{other}] not allowed, only [noop, index, delete] \
                             are allowed"
                        ),
                    );
                }
            }
            document = src;
            if let Some(new_id) = changed_id {
                id = new_id;
            } else if let crate::painless::Value::Map(m) = &ctx
                && crate::painless::value::map_get(m, &crate::painless::Value::str("_id"))
                    .map(|v| v.is_null())
                    .unwrap_or(false)
            {
                // an id set to nothing asks for one to be made up
                id = String::new();
            }
            scripted_routing = changed_routing;
        }
        // a document written by a walk is written the way any document is, so
        // the pipelines that would have run over it run over it here too: the
        // one the request named, and whatever the destination's own settings
        // and templates say. A processor that drops the document drops it
        // from the walk as well.
        match crate::api::ingest::ingest_for_write(
            &store,
            &to,
            &id,
            document.clone(),
            through.as_deref(),
            None,
        ) {
            Ok(Some(piped)) => document = piped.source,
            Ok(None) => {
                tally.noops += 1;
                continue;
            }
            Err(e) => {
                tally.failures.push(json!({
                    "index": to, "id": seen.id, "status": 400,
                    "cause": {"type": e.kind, "reason": e.reason},
                }));
                continue;
            }
        }
        // the destination is made on the first document written to it, so a
        // script that sends every document elsewhere, or drops them all,
        // leaves no empty index behind
        let st = match store.get(&to) {
            Some(st) => st,
            None => match store.ensure(&to) {
                Ok(st) => st,
                Err(_) => continue,
            },
        };
        let mut g = st.write();
        if !written.contains(&to) {
            written.push(to.clone());
        }
        if id.is_empty() {
            id = g.next_auto_id();
        }
        if let Some(r) = scripted_routing {
            g.routing.insert(id.clone(), r);
        }
        match routing.as_deref() {
            Some("discard") => {
                g.routing.remove(&id);
            }
            Some(named) if named.starts_with('=') => {
                g.routing.insert(id.clone(), named[1..].to_string());
            }
            _ => {}
        }
        let existed = crate::api::doc::exists_doc(&g, &id);
        let op = if create_only || op_asked == "create" { "create" } else { "index" };
        match write_doc_raw(&mut g, &id, document, op, None) {
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
                                "[{}]: version conflict, document already exists (current \
                                 version [{}])",
                                seen.id,
                                g.version_of(&seen.id)
                            ),
                            "index": to, "shard": "0", "index_uuid": "_na_",
                        },
                    }));
                    break;
                }
            }
        }
    }
    for name in &written {
        if let Some(st) = store.get(name) {
            let _ = st.write().refresh();
        }
    }
    drop(script);
    finish(&store, tally, started, &p, &from, batch_size(&p, &source)).await
}

/// Whether the write can meet the number of copies the caller asked for.
///
/// One node holds one copy of each shard, so a request that wants more than
/// one active copy waits for replicas that will never be assigned. It is told
/// so rather than left waiting, and the answer carries the timeout it named.
fn too_few_copies(store: &Store, name: &str, p: &Params) -> Option<Value> {
    let asked = match p.get("wait_for_active_shards").map(|v| v.to_string()) {
        Some(written) if written == "all" => store
            .get(name)
            .map(|st| st.read().numeric_setting("number_of_replicas").unwrap_or(0) + 1)
            .unwrap_or(1),
        Some(written) => written.parse::<u64>().ok()?,
        None => return None,
    };
    if asked <= 1 {
        return None;
    }
    let timeout = p.get("timeout").map(|v| v.to_string()).unwrap_or_else(|| "1m".to_string());
    Some(json!({
        "index": name, "id": "", "status": 503,
        "cause": {
            "type": "unavailable_shards_exception",
            "reason": format!(
                "[{name}][0] Not enough active copies to meet shard count of [{asked}] (have 1, \
                 needed {asked}). Timeout: [{timeout}], request: [BulkShardRequest]"
            ),
            "index": name, "shard": "0", "index_uuid": "_na_",
        },
    }))
}

/// Why an index may not be created on the fly, where it may not.
///
/// `action.auto_create_index` is either a flat yes or no, or a list of
/// patterns a new name has to match -- and a pattern written with a leading
/// `-` forbids the names it matches.
pub(crate) fn auto_create_complaint(store: &Store, name: &str) -> Option<Response> {
    let setting = store.cluster_setting("action.auto_create_index")?;
    let written = match &setting {
        Value::Bool(b) => b.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let refuse = |why: String| Some(err(StatusCode::BAD_REQUEST, "index_not_found_exception", why));
    if written == "false" {
        return refuse(format!("no such index [{name}] and [action.auto_create_index] is [false]"));
    }
    if written == "true" {
        return None;
    }
    for pattern in written.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()) {
        let (forbids, glob) = match pattern.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, pattern),
        };
        if !crate::store::glob_match(glob, name) {
            continue;
        }
        return match forbids {
            true => refuse(format!(
                "no such index [{name}] and [action.auto_create_index] contains [{pattern}] \
                 which forbids automatic creation of the index"
            )),
            false => None,
        };
    }
    refuse(format!(
        "no such index [{name}] and [action.auto_create_index] ([{written}]) doesn't match"
    ))
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
///
/// `_source` is written here the way a search writes it -- a name, a list of
/// names, or an object of includes and excludes -- so it is read the same way.
fn only_these(document: &Value, fields: &Value) -> Value {
    match crate::api::apply_source_selector(document, fields) {
        Value::Null => json!({}),
        kept => kept,
    }
}

/// The answer, or the name of the task it would have been.
async fn finish(
    store: &Store,
    tally: Tally,
    started: std::time::Instant,
    p: &Params,
    read: &str,
    per_batch: usize,
) -> Response {
    // the walk reads a page at a time, and says how many pages it read
    let batches = tally.total.div_ceil(per_batch).max(1);
    // a walk told how many documents a second it may write waits between its
    // batches until it has held to that rate
    let rate = p.get("requests_per_second").and_then(|v| v.parse::<f64>().ok()).unwrap_or(-1.0);
    let throttled_millis = match rate > 0.0 {
        true => ((batches - 1) as f64 * per_batch as f64 / rate * 1000.0) as u64,
        false => 0,
    };
    // a walk that is being waited for holds to the rate before it answers; a
    // walk running as a task is left to be rethrottled instead
    if throttled_millis > 0 && !as_task(p) {
        tokio::time::sleep(std::time::Duration::from_millis(throttled_millis)).await;
    }
    let mut answer = tally.answer(started.elapsed().as_millis(), batches);
    answer["throttled_millis"] = json!(throttled_millis);
    answer["requests_per_second"] = json!(rate);
    // a walk asked to be sliced is one walk here, and says so slice by slice
    // `auto` means one slice per shard of the index the walk read
    let asked = match p.get("slices").map(|v| v.as_str()) {
        Some("auto") => store
            .resolve(read)
            .first()
            .and_then(|name| store.get(name))
            .map(|st| st.read().shard_count() as usize)
            .filter(|n| *n > 1),
        other => other.and_then(|v| v.parse::<usize>().ok()).filter(|n| *n > 1),
    };
    if let Some(slices) = asked {
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
        store.remember_task(&name, answer.clone());
        // a task outlives the request that started it, so what it did is kept
        // where anyone can read it back
        if store.ensure(".tasks").is_ok()
            && let Some(st) = store.get(".tasks")
        {
            let mut g = st.write();
            let record = json!({
                "completed": true,
                "task": {
                    "node": "node-0", "id": 1, "type": "transport",
                    "action": "indices:data/write/by_query", "description": name,
                    "start_time_in_millis": 0, "running_time_in_nanos": 0, "cancellable": true,
                },
                "response": answer,
            });
            let _ = write_doc_raw(&mut g, &name, record, "index", None);
            let _ = g.refresh();
        }
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

/// What an update script asked for: the operation and the document as it
/// left `ctx`, or why the request cannot be honoured.
fn scripted_change(
    ctx: &crate::painless::Value,
    id: &str,
) -> std::result::Result<(&'static str, Value), String> {
    let extra = crate::painless::contexts::ctx_extra_keys(ctx);
    if let Some(junk) = extra.first() {
        return Err(format!("Invalid fields added to context [{junk}]"));
    }
    let (op, source, changed_id, _) = crate::painless::contexts::read_ctx(ctx)?;
    if changed_id.as_deref().map(|c| c != id).unwrap_or(false) {
        return Err("Modifying [_id] not allowed".into());
    }
    Ok(match op.as_str() {
        "noop" | "none" => ("noop", source),
        "delete" => ("delete", source),
        "index" => ("index", source),
        other => {
            return Err(format!(
                "Operation type [{other}] not allowed, only [noop, index, delete] are allowed"
            ));
        }
    })
}
