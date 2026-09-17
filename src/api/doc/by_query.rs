//! Writing over the documents a query finds.
//!
//! `_delete_by_query`, `_update_by_query` and `_reindex` are the same walk:
//! run a query, take what it found, and write. They answer with the same
//! tally -- how many were looked at, how many were written, and what went
//! wrong -- so the tally is built once here.
//!
//! A walk is a task. It reads what its query found, then writes it a batch at
//! a time, holding to the rate it was given between batches, and stops when it
//! is cancelled. Each batch runs off the request runtime and takes an index's
//! lock only for the writes themselves: a walk that held its thread, or the
//! lock, for the whole of its run kept a single write, a bulk or a count
//! waiting seconds behind it, where OpenSearch lets them in between the
//! walk's batches. A walk asked not to be waited for hands back its task id at
//! once and leaves its result in `.tasks`.

use super::*;
use crate::tasks::{NewTask, Task};
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// One document, as the walk saw it.
pub(crate) struct Seen {
    index: String,
    id: String,
    source: Value,
    /// where the document stood when the walk read it
    seq_no: Option<u64>,
}

/// Which of the three walks a request is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Update,
    Delete,
    Reindex,
}

impl Kind {
    /// The action the walk's task runs under, as OpenSearch names it.
    fn action(self) -> &'static str {
        match self {
            Kind::Update => "indices:data/write/update/byquery",
            Kind::Delete => "indices:data/write/delete/byquery",
            Kind::Reindex => "indices:data/write/reindex",
        }
    }

    /// The figures the answer to a waited-for walk leaves out: a delete says
    /// nothing of documents made or changed, and an update makes none. The
    /// task's status and its stored result carry every figure.
    fn unsaid(self) -> &'static [&'static str] {
        match self {
            Kind::Delete => &["created", "updated"],
            Kind::Update => &["created"],
            Kind::Reindex => &[],
        }
    }
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
    /// What a refused write really was.
    ///
    /// These walks pass no version and no `if_seq_no`, so a version conflict
    /// is not something they can produce: every refusal here is a mapping
    /// that would not take the document, a field limit, an id too long, or an
    /// index held still. Counting them all as conflicts told the caller the
    /// documents were already as asked -- and with `conflicts=proceed` it
    /// answered 200 with no failures at all, having written nothing.
    fn note_refusal(&mut self, index: &str, id: &str, refusal: &Response, proceeding: bool) {
        let (kind, reason) = match refusal.extensions().get::<crate::api::ErrorKind>() {
            Some(e) => (e.kind.clone(), e.reason.clone()),
            None => ("illegal_state_exception".to_string(), "the write was refused".to_string()),
        };
        if kind == "version_conflict_engine_exception" {
            self.version_conflicts += 1;
            // `conflicts: proceed` is the caller saying a conflict is not a
            // failure: it is counted and the walk goes on, and the answer
            // lists no failure for it. Listing it anyway made the walk answer
            // 409 for something the caller asked to be told about in the
            // count instead.
            if proceeding {
                return;
            }
        }
        let status = refusal.status().as_u16();
        self.failures.push(json!({
            "index": index, "id": id, "status": status,
            "cause": {
                "type": kind, "reason": reason,
                "index": index, "shard": "0", "index_uuid": "_na_",
            },
        }));
    }

    /// A document that was written to since the walk read it, standing now
    /// at `now`.
    fn note_conflict(&mut self, seen: &Seen, now: u64) {
        let id = &seen.id;
        let seq = seen.seq_no.unwrap_or(0);
        self.failures.push(json!({
            "index": seen.index, "id": id, "status": 409,
            "cause": {
                "type": "version_conflict_engine_exception",
                "reason": format!(
                    "[{id}]: version conflict, required seqNo [{seq}], primary term [1]. \
                     current document has seqNo [{now}] and primary term [1]"
                ),
                "index": seen.index, "shard": "0", "index_uuid": "_na_",
            },
        }));
    }

    /// Add what one batch did to what the walk had done before it. The total
    /// is the walk's, set once when it has read what it will write.
    fn absorb(&mut self, batch: Tally) {
        self.created += batch.created;
        self.updated += batch.updated;
        self.deleted += batch.deleted;
        self.noops += batch.noops;
        self.version_conflicts += batch.version_conflicts;
        self.failures.extend(batch.failures);
    }
}

/// Why a walk's batches are too large for what it reads, if they are.
///
/// The walk writes a batch at a time, the way the reference scrolls, so a
/// batch -- not the number of documents walked -- is what the index's
/// `max_result_window` bounds. It is judged before the walk starts, so a
/// request sent off as a task is refused as the waited-for one is.
fn batch_refusal(store: &Store, expr: &str, batch: usize) -> Option<Response> {
    let window = store
        .resolve_open(expr)
        .iter()
        .filter_map(|n| store.get(n))
        .filter_map(|st| st.read().numeric_setting("max_result_window"))
        .min()
        .unwrap_or(10_000);
    if batch as u64 > window {
        let reason = format!(
            "Batch size is too large, size must be less than or equal to: [{window}] but was \
             [{batch}]. Scroll batch sizes cost as much memory as result windows so they are \
             controlled by the [index.max_result_window] index level setting."
        );
        // the reference fails the first shard's scroll with it, and says so
        // the way a failed search phase does
        let cause = json!({"type": "illegal_argument_exception", "reason": reason});
        let mut caused_by = cause.clone();
        caused_by["caused_by"] = cause.clone();
        let index = store.resolve_open(expr).into_iter().next().unwrap_or_default();
        let body = json!({
            "error": {
                "root_cause": [cause.clone()],
                "type": "search_phase_execution_exception",
                "reason": "all shards failed",
                "phase": "query",
                "grouped": true,
                "failed_shards": [{"shard": 0, "index": index,
                    "node": crate::cluster::identity().id.as_str(), "reason": cause}],
                "caused_by": caused_by,
            },
            "status": 400,
        });
        return Some((StatusCode::BAD_REQUEST, axum::Json(body)).into_response());
    }
    None
}

/// Every document a query finds, as `(index, id, source)`.
///
/// The walk reads them all before it writes any: writing while the reader is
/// still open would have it read what the walk itself had just written. With
/// no limit named, the limit is everything the query matches -- a walk that
/// stopped at the first ten thousand left the rest of an index unchanged and
/// said it was done. The request's `routing` and `preference` keep the walk
/// to the shards they name, as they keep a search.
fn found(
    store: &Store,
    expr: &str,
    body: &Value,
    limit: Option<usize>,
    p: &Params,
) -> std::result::Result<Vec<Seen>, Response> {
    let mut asked = Params::new();
    for key in ["routing", "preference"] {
        if let Some(v) = p.get(key) {
            asked.insert(key.into(), v.clone());
        }
    }
    let query = body.get("query").cloned().unwrap_or_else(|| json!({"match_all": {}}));
    let limit = match limit {
        Some(n) => n,
        None => {
            let counted = json!({"query": query, "size": 0, "track_total_hits": true});
            crate::search::run(store, expr, &counted, &asked)?.total as usize
        }
    };
    if limit == 0 {
        return Ok(Vec::new());
    }
    // the sequence number each document stood at is what makes a write
    // conditional: one written since is a conflict, not a document to write
    let mut request =
        json!({"query": query, "size": limit, "_source": true, "seq_no_primary_term": true});
    if let Some(sort) = body.get("sort") {
        request["sort"] = sort.clone();
    }
    let answer =
        crate::search::as_the_server(|| crate::search::run(store, expr, &request, &asked))?;
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
fn max_docs(p: &Params, body: &Value) -> Option<usize> {
    p.get("max_docs").or_else(|| p.get("size")).and_then(|v| v.parse::<usize>().ok()).or_else(
        || {
            body.get("max_docs")
                .or_else(|| body.get("size"))
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
        },
    )
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
    let authority = host
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    // A URL may carry a user name and password before the host, and what is
    // in front of the `@` is not where the request goes: with an allowlist
    // entry naming no port, `http://allowed.host:1@169.254.169.254` read as
    // host `allowed.host` and port `1@169.254.169.254`, matched `*`, and
    // fetched from the address after the `@`. What is judged is where the
    // request will actually go.
    match authority.rsplit_once('@') {
        Some((_, real)) => real.to_string(),
        None => authority.to_string(),
    }
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
    let Ok(listed) = std::env::var("VELOSEARCH_REINDEX_ALLOWLIST") else {
        return false;
    };
    let (host, port) = match named.rsplit_once(':') {
        // a port is digits: anything else is not an authority this node will
        // judge, whatever it might mean to a URL parser somewhere else
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (h, p),
        Some(_) => return false,
        None => (named, ""),
    };
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
    let host =
        remote.get("host").and_then(|v| v.as_str()).unwrap_or_default().trim_end_matches('/');
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
            // A redirect is a second address, and the allowlist judged the
            // first. An allowlisted host answering `302 Location:
            // http://169.254.169.254/...` had this fetch the second address
            // -- with the credentials the caller gave for the first. The
            // redirect is not followed; it is answered as what it is.
            .max_redirects(0)
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
            Ok(answer) if answer.status().is_redirection() => {
                // said plainly rather than as "the body was not JSON": the
                // host that was allowed is sending this somewhere else, and
                // where it points was never judged by the allowlist
                Err(remote_failure(format!(
                    "the remote answered {} and pointed elsewhere; a redirect is not followed",
                    answer.status()
                )))
            }
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
        let hits: Vec<Value> =
            page.pointer("/hits/hits").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        if hits.is_empty() {
            break;
        }
        // a page that adds nothing is a walk that is not moving: a remote
        // answering hits with no `_id` scrolled for ever, one blocking
        // thread at a time
        let had = out.len();
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
        if out.len() >= limit || out.len() == had {
            break;
        }
        let Some(held) = scroll.clone() else { break };
        page = call(format!("{host}/_search/scroll"), json!({"scroll": "5m", "scroll_id": held}))?;
        scroll = page.get("_scroll_id").and_then(|v| v.as_str()).map(|s| s.to_string());
    }
    // the other cluster should not be left holding a context this walk is done
    // with, whether or not it minds
    if let Some(held) = scroll {
        // with the same bound as every other call to this remote: without
        // one, a remote that stopped answering held this blocking thread for
        // as long as the operating system would wait
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_millis(timeout.max(1.0) as u64)))
            .max_redirects(0)
            .build()
            .into();
        let _ = agent.delete(&format!("{host}/_search/scroll?scroll_id={held}")).call();
    }
    Ok(out)
}

/// A remote that could not be read, in the words a client expects.
fn remote_failure(reason: String) -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, "connect_exception", reason)
}

/// Why a walk that writes may not run over these indices, if it may not.
fn change_refusal_for(store: &Store, expr: &str) -> Option<Response> {
    for name in store.resolve(expr) {
        let Some(st) = store.get(&name) else { continue };
        let refusal = st.read().change_refusal();
        if let Some((kind, why)) = refusal {
            let status = crate::store::IdxState::refusal_status(kind, &why);
            return Some(err(status, kind, why));
        }
    }
    None
}

/// A rate as a request writes it: a positive number of documents a second,
/// or anything else for no limit at all.
fn rate_of(written: Option<&str>) -> f64 {
    match written.and_then(|v| v.parse::<f64>().ok()) {
        Some(r) if r > 0.0 => r,
        _ => f64::INFINITY,
    }
}

/// A rate as OpenSearch writes it back, where no limit is `-1`.
fn rate_json(rate: f64) -> Value {
    if rate.is_finite() { json!(rate) } else { json!(-1.0) }
}

/// How fast a walk may go, and the wait it is in between batches, if any.
struct Throttle {
    rate: f64,
    /// when the wait in progress ends, and how long it was set for
    due: Option<(Instant, Duration)>,
}

impl Throttle {
    /// How long the next batch waits: a batch of `size` documents started at
    /// `start` has earned the next one its turn after `size / rate` seconds,
    /// less however long the batch itself took.
    fn wait_after(&self, start: Instant, size: usize) -> Duration {
        if !self.rate.is_finite() || size == 0 {
            return Duration::ZERO;
        }
        let earned = Duration::try_from_secs_f64(size as f64 / self.rate).unwrap_or(FOREVER);
        match start.checked_add(earned) {
            Some(at) => at.saturating_duration_since(Instant::now()),
            None => FOREVER,
        }
    }
}

/// A wait long enough to be no limit at all, which a rate as small as the
/// suite's `0.00000001` asks for.
const FOREVER: Duration = Duration::from_secs(100 * 365 * 24 * 3600);

/// The figures of a walk, or of one slice of it, at one moment.
#[derive(Clone, Default)]
struct Figures {
    slice_id: Option<usize>,
    total: u64,
    updated: u64,
    created: u64,
    deleted: u64,
    batches: u64,
    version_conflicts: u64,
    noops: u64,
    throttled_nanos: u64,
    rate: f64,
    canceled: Option<String>,
    throttled_until_nanos: u64,
}

impl Figures {
    /// The figures in the order OpenSearch writes a walk's status. `human`
    /// adds the two waits written out, which a stored result carries.
    fn write(&self, human: bool) -> serde_json::Map<String, Value> {
        let mut o = serde_json::Map::new();
        if let Some(id) = self.slice_id {
            o.insert("slice_id".into(), json!(id));
        }
        o.insert("total".into(), json!(self.total));
        o.insert("updated".into(), json!(self.updated));
        o.insert("created".into(), json!(self.created));
        o.insert("deleted".into(), json!(self.deleted));
        o.insert("batches".into(), json!(self.batches));
        o.insert("version_conflicts".into(), json!(self.version_conflicts));
        o.insert("noops".into(), json!(self.noops));
        o.insert("retries".into(), json!({"bulk": 0, "search": 0}));
        if human {
            o.insert("throttled".into(), json!(crate::tasks::time_text(self.throttled_nanos)));
        }
        o.insert("throttled_millis".into(), json!(self.throttled_nanos / 1_000_000));
        o.insert("requests_per_second".into(), rate_json(self.rate));
        if let Some(why) = &self.canceled {
            o.insert("canceled".into(), json!(why));
        }
        if human {
            o.insert(
                "throttled_until".into(),
                json!(crate::tasks::time_text(self.throttled_until_nanos)),
            );
        }
        o.insert("throttled_until_millis".into(), json!(self.throttled_until_nanos / 1_000_000));
        o
    }

    /// Fold a finished slice into the walk's figures.
    fn add(&mut self, other: &Figures) {
        self.total += other.total;
        self.updated += other.updated;
        self.created += other.created;
        self.deleted += other.deleted;
        self.batches += other.batches;
        self.version_conflicts += other.version_conflicts;
        self.noops += other.noops;
        self.throttled_nanos += other.throttled_nanos;
        self.rate += other.rate;
        self.throttled_until_nanos = self.throttled_until_nanos.max(other.throttled_until_nanos);
        if self.canceled.is_none() {
            self.canceled = other.canceled.clone();
        }
    }
}

/// What one walk, or one slice of a walk, has done so far.
///
/// Shared between the walk, which adds to it a batch at a time, and whoever
/// asks the task how it is getting on.
struct Progress {
    slice_id: Option<usize>,
    tally: parking_lot::Mutex<Tally>,
    batches: AtomicU64,
    throttle: parking_lot::Mutex<Throttle>,
    throttled_nanos: AtomicU64,
    canceled: parking_lot::Mutex<Option<String>>,
    /// woken when the rate changes, so a wait set at the old rate is judged
    /// again at the new one
    wake: tokio::sync::Notify,
}

impl Progress {
    fn new(slice_id: Option<usize>, rate: f64) -> Arc<Progress> {
        Arc::new(Progress {
            slice_id,
            tally: parking_lot::Mutex::new(Tally::default()),
            batches: AtomicU64::new(0),
            throttle: parking_lot::Mutex::new(Throttle { rate, due: None }),
            throttled_nanos: AtomicU64::new(0),
            canceled: parking_lot::Mutex::new(None),
            wake: tokio::sync::Notify::new(),
        })
    }

    fn figures(&self) -> Figures {
        let t = self.tally.lock();
        let th = self.throttle.lock();
        let until = th
            .due
            .map(|(at, _)| at.saturating_duration_since(Instant::now()).as_nanos() as u64)
            .unwrap_or(0);
        Figures {
            slice_id: self.slice_id,
            total: t.total as u64,
            updated: t.updated as u64,
            created: t.created as u64,
            deleted: t.deleted as u64,
            batches: self.batches.load(Ordering::Relaxed),
            version_conflicts: t.version_conflicts as u64,
            noops: t.noops as u64,
            throttled_nanos: self.throttled_nanos.load(Ordering::Relaxed),
            rate: th.rate,
            canceled: self.canceled.lock().clone(),
            throttled_until_nanos: until,
        }
    }

    /// A new rate, as OpenSearch applies one: a slower rate waits for the
    /// next batch, and a faster one shortens the wait already under way in
    /// proportion -- to nothing, when the limit is lifted.
    fn set_rate(&self, written: f64) {
        let rate = if written > 0.0 { written } else { f64::INFINITY };
        let mut th = self.throttle.lock();
        let old = th.rate;
        th.rate = rate;
        if rate > old
            && let Some((at, _)) = th.due
        {
            let now = Instant::now();
            let left = at.saturating_duration_since(now);
            let scaled = match rate.is_finite() {
                true => {
                    Duration::try_from_secs_f64(left.as_secs_f64() * old / rate).unwrap_or(FOREVER)
                }
                false => Duration::ZERO,
            };
            th.due = Some((now.checked_add(scaled).unwrap_or(at), scaled));
        }
        drop(th);
        self.wake.notify_one();
    }

    /// Wait until the rate lets the next batch start, given when the last one
    /// started and how many documents it held. Answers false if the task was
    /// cancelled while it waited.
    async fn wait_turn(&self, task: &Task, start: Instant, size: usize) -> bool {
        // worked out and set under one hold of the lock, so a new rate
        // arriving in between cannot be missed by a wait set at the old one
        {
            let mut th = self.throttle.lock();
            let wait = th.wait_after(start, size);
            if !wait.is_zero() {
                th.due =
                    Some((Instant::now().checked_add(wait).unwrap_or_else(Instant::now), wait));
            }
        }
        loop {
            let Some((at, _)) = self.throttle.lock().due else { break };
            if task.is_cancelled() {
                self.throttle.lock().due = None;
                return false;
            }
            let now = Instant::now();
            if now >= at {
                break;
            }
            // the wait is woken early by a new rate or a cancel, and never
            // sleeps longer than a minute at a stretch: a far deadline is
            // judged again rather than handed to the timer whole
            let until = at.min(now + Duration::from_secs(60));
            tokio::select! {
                _ = tokio::time::sleep_until(until.into()) => {}
                _ = self.wake.notified() => {}
                _ = task.wake.notified() => {}
            }
        }
        // the wait counts once it has been served, at the length it ended up
        // being: a wait cut short by lifting the limit counts for nothing
        if let Some((_, served)) = self.throttle.lock().due.take() {
            self.throttled_nanos.fetch_add(served.as_nanos() as u64, Ordering::Relaxed);
        }
        !task.is_cancelled()
    }
}

impl crate::tasks::Work for Progress {
    fn status(&self) -> Option<Value> {
        Some(Value::Object(self.figures().write(false)))
    }

    fn rethrottle(&self, rate: f64) {
        self.set_rate(rate);
    }
}

/// A walk split into slices, as its task reports it.
///
/// Its status is the sum of the slices that have finished, with each slice's
/// own figures beside it and `null` for a slice still running -- which is
/// how OpenSearch reports a sliced walk, rather than a running total.
struct Sliced {
    slices: Vec<Arc<Progress>>,
    finished: parking_lot::Mutex<Vec<Option<Figures>>>,
}

impl crate::tasks::Work for Sliced {
    fn status(&self) -> Option<Value> {
        let done = self.finished.lock();
        let mut sum = Figures::default();
        for f in done.iter().flatten() {
            sum.add(f);
        }
        let mut o = sum.write(false);
        let each: Vec<Value> = done
            .iter()
            .map(|f| f.as_ref().map(|f| Value::Object(f.write(false))).unwrap_or(Value::Null))
            .collect();
        o.insert("slices".into(), json!(each));
        Some(Value::Object(o))
    }

    /// The new rate is shared between the slices still running.
    fn rethrottle(&self, rate: f64) {
        let done = self.finished.lock();
        let running: Vec<&Arc<Progress>> = self
            .slices
            .iter()
            .zip(done.iter())
            .filter(|(_, d)| d.is_none())
            .map(|(s, _)| s)
            .collect();
        if running.is_empty() {
            return;
        }
        let each = if rate > 0.0 { rate / running.len() as f64 } else { f64::INFINITY };
        for slice in running {
            slice.set_rate(each);
        }
    }
}

/// A walk that has been judged worth running.
struct Walk {
    kind: Kind,
    store: Store,
    p: Params,
    body: Value,
    /// the search it reads with: the body of a by-query walk, the `source`
    /// of a reindex
    search: Value,
    /// the indices it reads
    read: String,
    batch: usize,
    proceed: bool,
    /// the caller, whose filters and permissions the walk's reads and writes
    /// run under on whatever thread they run on
    caller: Option<crate::security::Caller>,
}

impl Walk {
    /// The task's description, in OpenSearch's words.
    fn description(&self) -> String {
        let read = format!("[{}]", self.read.split(',').collect::<Vec<_>>().join(", "));
        let script = self
            .body
            .get("script")
            .map(|s| format!(" updated with {}", script_text(s)))
            .unwrap_or_default();
        match self.kind {
            Kind::Update => format!("update-by-query {read}{script}"),
            Kind::Delete => format!("delete-by-query {read}"),
            Kind::Reindex => {
                let to = self.body.pointer("/dest/index").and_then(|v| v.as_str()).unwrap_or("");
                let from = match self.search.pointer("/remote/host").and_then(|v| v.as_str()) {
                    Some(host) => format!("[host={host}]{read}"),
                    None => read,
                };
                format!("reindex from {from}{script} to [{to}]")
            }
        }
    }

    /// The parameters the walk's read keeps: a by-query walk is kept to the
    /// shards its request's `routing` and `preference` name; a reindex names
    /// its source in the body, and reads all of it.
    fn routed(&self) -> Params {
        match self.kind {
            Kind::Reindex => Params::new(),
            _ => self.p.clone(),
        }
    }

    /// How many documents the walk may write, if the request said.
    fn wanted(&self) -> Option<usize> {
        max_docs(&self.p, &self.body)
    }
}

/// A script as Java prints one, which is how a task describes what it runs.
fn script_text(spec: &Value) -> String {
    let (kind, lang, code) = match spec {
        Value::String(s) => ("inline", "painless".to_string(), s.clone()),
        other => {
            let stored = other.get("id").and_then(|v| v.as_str());
            let code = other
                .get("source")
                .or_else(|| other.get("inline"))
                .and_then(|v| v.as_str())
                .or(stored)
                .unwrap_or_default()
                .to_string();
            let lang = match stored {
                Some(_) => other.get("lang").and_then(|v| v.as_str()).unwrap_or("null"),
                None => other.get("lang").and_then(|v| v.as_str()).unwrap_or("painless"),
            };
            (if stored.is_some() { "stored" } else { "inline" }, lang.to_string(), code)
        }
    };
    let params: Vec<String> = spec
        .get("params")
        .and_then(|v| v.as_object())
        .into_iter()
        .flatten()
        .map(|(k, v)| match v {
            Value::String(s) => format!("{k}={s}"),
            other => format!("{k}={other}"),
        })
        .collect();
    format!(
        "Script{{type={kind}, lang='{lang}', idOrCode='{code}', options={{}}, params={{{}}}}}",
        params.join(", ")
    )
}

/// Run `f` as the caller of the request that started the walk.
fn as_caller<R>(caller: Option<crate::security::Caller>, f: impl FnOnce() -> R) -> R {
    match caller {
        Some(c) => crate::security::layer::CALLER.sync_scope(c, f),
        None => f(),
    }
}

/// Everything a walk's query found, read before anything is written.
fn read_walk(walk: &Walk) -> std::result::Result<Vec<Seen>, Response> {
    match walk.search.get("remote") {
        Some(remote) if walk.kind == Kind::Reindex => found_remote(
            remote,
            &walk.read,
            &walk.search,
            walk.wanted().unwrap_or(usize::MAX),
            walk.batch,
        ),
        // the walk reads everything it will write in one search, which is
        // past the result window a caller's search is held to: the window
        // bounds what a caller may ask the node to hold, and a walk the node
        // runs for itself holds it a batch at a time on the way out
        _ => crate::search::as_the_server(|| {
            found(&walk.store, &walk.read, &walk.search, walk.wanted(), &walk.routed())
        }),
    }
}

/// The slice a document falls in, the way OpenSearch divides a scroll.
///
/// With no more slices than shards, a slice takes whole shards. With more, each
/// shard's documents are shared between the slices that shard holds, by a hash
/// of the id.
fn slice_of(store: &Store, seen: &Seen, max: usize) -> usize {
    let (shard, shards) = store
        .get(&seen.index)
        .map(|st| {
            let g = st.read();
            (g.shard_of_doc(&seen.id) as usize, (g.shard_count() as usize).max(1))
        })
        .unwrap_or((0, 1));
    if max <= shards {
        return shard % max;
    }
    let mut in_shard = max / shards;
    if max % shards > shard {
        in_shard += 1;
    }
    let hash = seen
        .id
        .bytes()
        .fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    shard + shards * (hash % in_shard as u64) as usize
}

/// How many slices a request asked for, with `auto` one per shard of what it
/// reads.
fn slices_asked(store: &Store, p: &Params, read: &str) -> usize {
    match p.get("slices").map(|v| v.as_str()) {
        Some("auto") => store
            .resolve(read)
            .iter()
            .filter_map(|name| store.get(name))
            .map(|st| st.read().shard_count() as usize)
            .min()
            .unwrap_or(1)
            .max(1),
        Some(n) => n.parse::<usize>().ok().filter(|n| *n > 0).unwrap_or(1),
        None => 1,
    }
}

/// How a batch left the walk.
enum After {
    /// on to the next batch
    Go,
    /// the batch is done and the walk stops here: something in it failed, or
    /// conflicted when the walk was told to abort on a conflict
    Stop,
    /// the walk cannot go on at all, and this is the error that says why
    Fail(Response),
}

/// What a slice keeps from one batch to the next.
#[derive(Default)]
struct Written {
    /// the indices written to, refreshed at the end when the request asked
    touched: BTreeSet<String>,
    /// the destinations a reindex script named that have been judged for
    /// this caller: judged once per name, not once per document
    judged: HashSet<String>,
}

/// How a walk ended.
enum Ended {
    Done { took: u128, figures: Figures, slices: Option<Vec<Figures>>, failures: Vec<Value> },
    Failed(Response),
}

/// Run a walk to its end.
async fn drive(walk: Arc<Walk>, task: Arc<Task>, slices: usize, rate: f64) -> Ended {
    let started = Instant::now();
    // what the walk will report is in place before it reads anything: a
    // rethrottle or a cancel that arrives while the documents are still being
    // read -- which a reindex from another cluster can take a while over --
    // found no work to change, and the walk went on at the rate it was given
    let each_rate = if rate.is_finite() { rate / slices.max(1) as f64 } else { rate };
    let progresses: Vec<Arc<Progress>> = match slices {
        0 | 1 => vec![Progress::new(None, rate)],
        n => (0..n).map(|i| Progress::new(Some(i), each_rate)).collect(),
    };
    let sliced = Arc::new(Sliced {
        slices: progresses.clone(),
        finished: parking_lot::Mutex::new(vec![None; progresses.len()]),
    });
    match slices {
        0 | 1 => task.attach(progresses[0].clone()),
        _ => task.attach(sliced.clone()),
    }
    let reader = walk.clone();
    let read = tokio::task::spawn_blocking(move || {
        as_caller(reader.caller.clone(), || read_walk(&reader))
    })
    .await;
    let hits = match read {
        Ok(Ok(hits)) => hits,
        Ok(Err(e)) => return Ended::Failed(e),
        Err(e) => return Ended::Failed(remote_failure(format!("{e}"))),
    };
    if slices <= 1 {
        let progress = progresses[0].clone();
        let end = run_slice(walk.clone(), progress.clone(), task.clone(), hits).await;
        refresh_touched(&walk, &end.written).await;
        if let Some(failed) = end.failed {
            return Ended::Failed(failed);
        }
        return Ended::Done {
            took: started.elapsed().as_millis(),
            figures: progress.figures(),
            slices: None,
            failures: std::mem::take(&mut progress.tally.lock().failures),
        };
    }
    // each slice is a task of its own under the walk's, with its share of the
    // documents and of the rate
    let mut parts: Vec<Vec<Seen>> = (0..slices).map(|_| Vec::new()).collect();
    for seen in hits {
        let at = slice_of(&walk.store, &seen, slices);
        parts[at].push(seen);
    }
    let mut running = Vec::new();
    for (i, part) in parts.into_iter().enumerate() {
        let child = crate::tasks::register(NewTask {
            action: walk.kind.action(),
            description: task.description.clone(),
            cancellable: true,
            parent: Some(task.id),
            headers: task.headers.clone(),
        });
        child.attach(progresses[i].clone());
        let (walk, progress, sliced) = (walk.clone(), progresses[i].clone(), sliced.clone());
        running.push(tokio::spawn(async move {
            let end = run_slice(walk, progress.clone(), child.0.clone(), part).await;
            sliced.finished.lock()[i] = Some(progress.figures());
            drop(child);
            end
        }));
    }
    let mut failed = None;
    let mut written = Written::default();
    for handle in running {
        match handle.await {
            Ok(end) => {
                written.touched.extend(end.written.touched);
                if failed.is_none() {
                    failed = end.failed;
                }
            }
            Err(e) => {
                failed.get_or_insert_with(|| remote_failure(format!("{e}")));
            }
        }
    }
    refresh_touched(&walk, &written).await;
    if let Some(failed) = failed {
        return Ended::Failed(failed);
    }
    let mut figures = Figures::default();
    let mut failures = Vec::new();
    for p in &progresses {
        figures.add(&p.figures());
        failures.extend(std::mem::take(&mut p.tally.lock().failures));
    }
    Ended::Done {
        took: started.elapsed().as_millis(),
        figures,
        slices: Some(progresses.iter().map(|p| p.figures()).collect()),
        failures,
    }
}

/// How one slice ended.
struct SliceEnd {
    failed: Option<Response>,
    written: Written,
}

/// Write one slice's documents, a batch at a time.
async fn run_slice(
    walk: Arc<Walk>,
    progress: Arc<Progress>,
    task: Arc<Task>,
    hits: Vec<Seen>,
) -> SliceEnd {
    progress.tally.lock().total = hits.len();
    let mut left = hits.into_iter();
    let mut written = Written::default();
    let mut last = (Instant::now(), 0usize);
    let mut failed = None;
    loop {
        let batch: Vec<Seen> = left.by_ref().take(walk.batch).collect();
        // the wait comes before every read of a batch, the empty one that
        // ends the walk included, as it does in OpenSearch
        if !progress.wait_turn(&task, last.0, last.1).await {
            *progress.canceled.lock() = Some("by user request".into());
            break;
        }
        if batch.is_empty() {
            break;
        }
        let started = Instant::now();
        let size = batch.len();
        progress.batches.fetch_add(1, Ordering::Relaxed);
        // what the batch writes is copied to the index's other copies once
        // the batch is done, as a request's writes are
        let writes: crate::cluster::replication::Writes = Default::default();
        let (w, noted) = (walk.clone(), writes.clone());
        let joined = tokio::task::spawn_blocking(move || {
            let mut tally = Tally::default();
            let after = crate::cluster::replication::WRITES.sync_scope(noted, || {
                as_caller(w.caller.clone(), || write_batch(&w, &mut written, batch, &mut tally))
            });
            (after, tally, written)
        })
        .await;
        let (after, tally, back) = match joined {
            Ok(done) => done,
            Err(e) => {
                failed = Some(remote_failure(format!("{e}")));
                written = Written::default();
                break;
            }
        };
        written = back;
        progress.tally.lock().absorb(tally);
        let ops = std::mem::take(&mut *writes.lock());
        if !ops.is_empty() {
            let refresh = walk.p.get("refresh").cloned().unwrap_or_default();
            let _ =
                crate::cluster::replication::finish(StatusCode::OK.into_response(), ops, &refresh)
                    .await;
        }
        last = (started, size);
        match after {
            After::Go => {}
            After::Stop => break,
            After::Fail(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    SliceEnd { failed, written }
}

/// Refresh what a walk wrote, when the request asked for it.
///
/// A walk used to refresh whatever it touched whether asked or not, so a
/// search straight after it saw its writes -- and a second walk started
/// before anything refreshed saw none of the conflicts it should have.
async fn refresh_touched(walk: &Walk, written: &Written) {
    if !flag(&walk.p, "refresh") {
        return;
    }
    // what the walk was pointed at is refreshed whether or not it wrote
    // anything there: a walk whose every document conflicted still leaves
    // the caller's next search seeing the writes it conflicted with
    let mut names = written.touched.clone();
    match walk.kind {
        Kind::Reindex => {
            if let Some(to) = walk.body.pointer("/dest/index").and_then(|v| v.as_str()) {
                names.extend(walk.store.resolve(to));
            }
        }
        _ => names.extend(walk.store.resolve(&walk.read)),
    }
    let store = walk.store.clone();
    let _ = tokio::task::spawn_blocking(move || {
        for name in names {
            if let Some(st) = store.get(&name) {
                let _ = st.write().refresh();
            }
        }
    })
    .await;
}

/// One batch, written.
fn write_batch(walk: &Walk, written: &mut Written, batch: Vec<Seen>, tally: &mut Tally) -> After {
    let after = match walk.kind {
        Kind::Delete => delete_batch(walk, written, batch, tally),
        Kind::Update => update_batch(walk, written, batch, tally),
        Kind::Reindex => reindex_batch(walk, written, batch, tally),
    };
    // a batch is answered for like a bulk: what it wrote is on disk before
    // the walk counts it done
    for name in &written.touched {
        if let Some(st) = walk.store.get(name)
            && let Err(why) = st.write().sync_translog()
        {
            return After::Fail(err(StatusCode::INTERNAL_SERVER_ERROR, "translog_exception", why));
        }
    }
    after
}

/// The script a walk runs, compiled for one batch. Painless values are not
/// shared between threads, and a batch may run on any of them; the request
/// compiled it once already to refuse one that does not compile.
fn batch_script(
    walk: &Walk,
) -> std::result::Result<Option<crate::painless::contexts::Compiled>, Response> {
    match walk.body.get("script") {
        Some(spec) => {
            match crate::painless::contexts::Compiled::of(spec, &|id| walk.store.stored_script(id))
            {
                Ok(c) => Ok(Some(c)),
                Err(e) if e.kind == "compile error" => Err(crate::api::compile_failure(e)),
                Err(e) => Err(crate::api::script_failure(e)),
            }
        }
        None => Ok(None),
    }
}

fn now_millis_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A conflict found at the write: counted, and a failure that stops the walk
/// after this batch unless the walk was told to proceed.
fn conflicted(walk: &Walk, tally: &mut Tally, seen: &Seen, now: u64, after: &mut After) {
    tally.version_conflicts += 1;
    if !walk.proceed {
        tally.note_conflict(seen, now);
        *after = After::Stop;
    }
}

fn delete_batch(walk: &Walk, written: &mut Written, batch: Vec<Seen>, tally: &mut Tally) -> After {
    let mut after = After::Go;
    for seen in batch {
        let Some(st) = walk.store.get(&seen.index) else { continue };
        let mut g = st.write();
        if let Some(now) = written_since(&g, &seen) {
            conflicted(walk, tally, &seen, now, &mut after);
            continue;
        }
        let (_, status) = delete_doc(&mut g, &seen.id);
        written.touched.insert(seen.index.clone());
        if status == StatusCode::OK {
            tally.deleted += 1;
        } else {
            tally.noops += 1;
        }
    }
    after
}

fn update_batch(walk: &Walk, written: &mut Written, batch: Vec<Seen>, tally: &mut Tally) -> After {
    let script = match batch_script(walk) {
        Ok(s) => s,
        Err(e) => return After::Fail(e),
    };
    // `?pipeline=` names one every rewritten document goes through
    let through = walk.p.get("pipeline").cloned();
    let mut after = After::Go;
    for seen in batch {
        let Some(st) = walk.store.get(&seen.index) else { continue };
        // the script sees the document in `ctx` and may change it, leave it,
        // or have it deleted. It runs with no lock on the index: a script is
        // the slow part of a walk, and a write waiting for the lock waited
        // for every script before it.
        let mut next = seen.source.clone();
        let mut op = "index";
        if let Some(compiled) = &script {
            let (name, version) = {
                let g = st.read();
                (g.name.clone(), g.version_of(&seen.id))
            };
            let ctx = crate::painless::contexts::update_ctx(
                &name,
                &seen.id,
                version,
                &seen.source,
                now_millis_i64(),
                "index",
            );
            let mut runner =
                crate::painless::contexts::Runner::new(&compiled.params).with_ctx(ctx.clone());
            if let Err(e) = runner.run(&compiled.script) {
                return After::Fail(crate::search::search_script_failure_partial(e, &seen.index));
            }
            match scripted_change(&ctx, &seen.id) {
                Ok((changed_op, source)) => {
                    op = changed_op;
                    next = source;
                }
                Err(reason) => {
                    return After::Fail(err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        reason,
                    ));
                }
            }
        }
        match op {
            "noop" => {
                tally.noops += 1;
                continue;
            }
            "delete" => {
                let mut g = st.write();
                if let Some(now) = written_since(&g, &seen) {
                    conflicted(walk, tally, &seen, now, &mut after);
                    continue;
                }
                let _ = crate::api::doc::delete_doc(&mut g, &seen.id);
                written.touched.insert(seen.index.clone());
                tally.deleted += 1;
                continue;
            }
            _ => {}
        }
        // a document rewritten in place is written the way any document is,
        // so a pipeline the request named runs over it -- before the lock is
        // taken, since a pipeline may read the store
        if let Some(named) = &through {
            match crate::api::ingest::ingest_for_write(
                &walk.store,
                &seen.index,
                &seen.id,
                next,
                Some(named),
                None,
            ) {
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
                    after = After::Stop;
                    continue;
                }
            }
        }
        let mut g = st.write();
        // what the script produced was computed from the version this walk
        // read: a document written since is a conflict, not one to overwrite
        if let Some(now) = written_since(&g, &seen) {
            conflicted(walk, tally, &seen, now, &mut after);
            continue;
        }
        written.touched.insert(seen.index.clone());
        match write_doc_raw(&mut g, &seen.id, next, "index", None) {
            Ok(_) => tally.updated += 1,
            Err(refusal) => {
                // the refusal is reported as itself, and a failure ends the
                // walk once its batch is done, as a failed bulk item does in
                // OpenSearch whether or not the walk was told to proceed: a
                // document that was not written is not one that was right
                let before = tally.failures.len();
                tally.note_refusal(&seen.index, &seen.id, &refusal, walk.proceed);
                if tally.failures.len() > before {
                    after = After::Stop;
                }
            }
        }
    }
    after
}

fn reindex_batch(walk: &Walk, written: &mut Written, batch: Vec<Seen>, tally: &mut Tally) -> After {
    let store = &walk.store;
    let script = match batch_script(walk) {
        Ok(s) => s,
        Err(e) => return After::Fail(e),
    };
    let source = &walk.search;
    let dest = walk.body.get("dest").cloned().unwrap_or_else(|| json!({}));
    let remote = source.get("remote");
    let from = &walk.read;
    let Some(to_named) = dest.get("index").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return After::Stop;
    };
    // a document may be written only where it is not already, if asked
    let create_only = dest.get("op_type").and_then(|v| v.as_str()) == Some("create");
    let kept = source.get("_source").cloned();
    // `dest.pipeline` names a pipeline every document goes through on the way
    // in, the same one an index request would name in its URL
    let through = dest.get("pipeline").and_then(|v| v.as_str()).map(|s| s.to_string());
    // where a destination names a routing, it decides which shard each
    // document lands on: `=value` writes them all under one, `discard` drops
    // the one the source carried, and `keep` leaves it as it stands
    let routing = dest.get("routing").and_then(|v| v.as_str()).map(|s| s.to_string());
    // the destination the request named was judged before the walk began
    written.judged.insert(to_named.clone());
    let mut after = After::Go;
    for seen in batch {
        let mut document = seen.source.clone();
        if let Some(fields) = kept.as_ref() {
            document = only_these(&document, fields);
        }
        // the script may send the document elsewhere, rename it, route it,
        // or say it is not to be written at all
        let mut to = to_named.clone();
        let mut id = seen.id.clone();
        let mut scripted_routing: Option<String> = None;
        let mut op_asked = "index";
        if let Some(compiled) = &script {
            let ctx = crate::painless::contexts::update_ctx(
                &to,
                &id,
                1,
                &document,
                now_millis_i64(),
                "index",
            );
            let mut runner =
                crate::painless::contexts::Runner::new(&compiled.params).with_ctx(ctx.clone());
            if let Err(e) = runner.run(&compiled.script) {
                return After::Fail(crate::search::search_script_failure_partial(e, &seen.index));
            }
            let extra = crate::painless::contexts::ctx_extra_keys(&ctx);
            if let Some(junk) = extra.first() {
                return After::Fail(err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!("Invalid fields added to context [{junk}]"),
                ));
            }
            let (op, src, changed_id, changed_routing) =
                match crate::painless::contexts::read_ctx(&ctx) {
                    Ok(read) => read,
                    Err(reason) => {
                        return After::Fail(err(
                            StatusCode::BAD_REQUEST,
                            "illegal_argument_exception",
                            reason,
                        ));
                    }
                };
            if let crate::painless::Value::Map(m) = &ctx
                && let Some(index_now) =
                    crate::painless::value::map_get(m, &crate::painless::Value::str("_index"))
            {
                let named = index_now.as_text();
                // a script may name another destination per document, and
                // that one is judged too -- once per name, not once per
                // document
                if named != to && !written.judged.contains(&named) {
                    // and it may not be the index being read: the request's
                    // own destination is checked against that before the walk
                    // begins, and a script naming the source went round it --
                    // rewriting in place the very thing the check exists for
                    if remote.is_none() && store.resolve(from).contains(&named) {
                        tally.failures.push(json!({
                            "index": named, "id": seen.id, "status": 400,
                            "cause": {
                                "type": "action_request_validation_exception",
                                "reason": format!(
                                    "Validation Failed: 1: reindex cannot write into an index \
                                     its reading from [{named}];"
                                ),
                            },
                        }));
                        continue;
                    }
                    if let Some(why) = crate::security::item_refusal(
                        store,
                        &["indices:data/write/index"],
                        &crate::security::layer::indices_for_expr(store, &named),
                    ) {
                        tally.failures.push(json!({
                            "index": named, "id": seen.id, "status": 403,
                            "cause": {"type": "security_exception", "reason": why},
                        }));
                        continue;
                    }
                    written.judged.insert(named.clone());
                }
                to = named;
            }
            match op.as_str() {
                "noop" => {
                    tally.noops += 1;
                    continue;
                }
                "delete" => {
                    // the document deleted is the one the script named
                    let target = changed_id.clone().unwrap_or_else(|| id.clone());
                    let mut done = false;
                    if let Some(st) = store.get(&to) {
                        let (answer, status) =
                            crate::api::doc::delete_doc(&mut st.write(), &target);
                        done = status.is_success();
                        written.touched.insert(to.clone());
                        if !done {
                            tally.failures.push(json!({
                                "index": to, "id": target,
                                "status": status.as_u16(),
                                "cause": answer.get("error").cloned().unwrap_or(json!({})),
                            }));
                        }
                    }
                    if !done {
                        continue;
                    }
                    tally.deleted += 1;
                    continue;
                }
                "index" | "create" => op_asked = if op == "create" { "create" } else { "index" },
                other => {
                    return After::Fail(err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        format!(
                            "Operation type [{other}] not allowed, only [noop, index, delete] \
                             are allowed"
                        ),
                    ));
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
            store,
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
                after = After::Stop;
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
        written.touched.insert(to.clone());
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
            Err(refusal) => {
                // what the destination refused, said as itself. A copy that
                // was not written is not a copy that was already there, and
                // a destination held still refused every one of them while
                // the answer said `version_conflicts` and `failures: []`.
                let before = tally.failures.len();
                tally.note_refusal(&to, &seen.id, &refusal, walk.proceed);
                if tally.failures.len() > before {
                    after = After::Stop;
                }
            }
        }
    }
    after
}

/// Start a walk: as a task that answers its id at once, or one the request
/// waits for.
async fn launch(walk: Walk, headers: &HeaderMap) -> Response {
    let slices = match walk.search.get("remote") {
        Some(_) => 1,
        None => slices_asked(&walk.store, &walk.p, &walk.read),
    };
    let rate = rate_of(walk.p.get("requests_per_second").map(|s| s.as_str()));
    let background = as_task(&walk.p);
    let task = crate::tasks::register(NewTask {
        action: walk.kind.action(),
        description: walk.description(),
        cancellable: true,
        parent: None,
        headers: crate::tasks::headers_of(headers),
    });
    let (p, kind, store, name) = (walk.p.clone(), walk.kind, walk.store.clone(), task.name());
    let walk = Arc::new(walk);
    // the walk runs as a task of its own, so it goes on to its end however
    // the request that started it ends -- a caller who hangs up has not
    // cancelled anything
    let job = tokio::spawn(async move {
        let ended = drive(walk, task.0.clone(), slices, rate).await;
        if background {
            keep_result(&store, &task, ended).await;
            return None;
        }
        Some(ended)
    });
    if background {
        return respond(&p, json!({ "task": name }));
    }
    match job.await {
        Ok(Some(Ended::Done { took, figures, slices, failures })) => {
            let human = flag(&p, "human");
            let mut answer = serde_json::Map::new();
            answer.insert("took".into(), json!(took as u64));
            answer.insert("timed_out".into(), json!(false));
            answer.extend(figures.write(human));
            if let Some(slices) = slices {
                let each: Vec<Value> = slices
                    .iter()
                    .map(|f| {
                        let mut o = f.write(human);
                        for unsaid in kind.unsaid() {
                            o.shift_remove(*unsaid);
                        }
                        Value::Object(o)
                    })
                    .collect();
                answer.insert("slices".into(), json!(each));
            }
            for unsaid in kind.unsaid() {
                answer.shift_remove(*unsaid);
            }
            // the walk answers with the worst status among its failures, as
            // OpenSearch's does: a conflict is a 409, a document the mapping
            // would not take a 400
            let worst = failures
                .iter()
                .filter_map(|f| f.get("status").and_then(|v| v.as_u64()))
                .max()
                .and_then(|s| StatusCode::from_u16(s as u16).ok())
                .filter(|s| s.as_u16() > 200)
                .unwrap_or(StatusCode::OK);
            answer.insert("failures".into(), json!(failures));
            let mut response = respond(&p, Value::Object(answer));
            *response.status_mut() = worst;
            response
        }
        Ok(Some(Ended::Failed(e))) => e,
        Ok(None) => err(StatusCode::INTERNAL_SERVER_ERROR, "exception", "the walk ended unseen"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "exception", format!("{e}")),
    }
}

/// Whether the request asked for the walk to be done in the background.
fn as_task(p: &Params) -> bool {
    p.get("wait_for_completion").map(|v| v == "false").unwrap_or(false)
}

/// Keep what a walk sent off as a task did, where its id finds it.
///
/// A task outlives the request that started it, so its result is written to
/// `.tasks` -- the task as it finished, and the answer or the error -- and a
/// caller asking for it later reads it back from there, as from OpenSearch.
async fn keep_result(store: &Store, task: &Task, ended: Ended) {
    let mut record = serde_json::Map::new();
    record.insert("completed".into(), json!(true));
    record.insert("task".into(), task.info(true, false));
    match ended {
        Ended::Done { took, figures, slices, failures } => {
            let mut answer = serde_json::Map::new();
            answer.insert("took".into(), json!(took as u64));
            answer.insert("timed_out".into(), json!(false));
            answer.extend(figures.write(true));
            if let Some(slices) = slices {
                let each: Vec<Value> =
                    slices.iter().map(|f| Value::Object(f.write(true))).collect();
                answer.insert("slices".into(), json!(each));
            }
            answer.insert("failures".into(), json!(failures));
            record.insert("response".into(), Value::Object(answer));
        }
        Ended::Failed(e) => {
            // the error is kept as the answer said it, without its status
            let bytes = axum::body::to_bytes(e.into_body(), usize::MAX).await.unwrap_or_default();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
            record.insert("error".into(), body.get("error").cloned().unwrap_or(body));
        }
    }
    crate::api::store_task_result(store, &task.name(), Value::Object(record)).await;
}

pub async fn delete_by_query(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    if let Some(complaint) = complaint(&p, &body).or_else(|| search_complaint(&body)) {
        return complaint;
    }
    // an index that takes no changes takes no walk over it either: the
    // deletes would each be refused and the answer would still say the walk
    // had run
    if let Some(refusal) = change_refusal_for(&store, &index) {
        return refusal;
    }
    // a walk that deletes has to be told what to delete
    if body.get("query").is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: query is missing;",
        );
    }
    if let Some(refusal) = batch_refusal(&store, &index, batch_size(&p, &body)) {
        return refusal;
    }
    if let Some(failure) = too_few_copies(&store, &index, &p) {
        return unavailable(failure);
    }
    let proceed = body.get("conflicts").and_then(|v| v.as_str()) == Some("proceed")
        || p.get("conflicts").map(|v| v == "proceed").unwrap_or(false);
    let walk = Walk {
        kind: Kind::Delete,
        batch: batch_size(&p, &body),
        search: body.clone(),
        read: index,
        body,
        store,
        p,
        proceed,
        caller: crate::security::layer::current_caller(),
    };
    launch(walk, &headers).await
}

pub async fn update_by_query(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    if let Some(complaint) = complaint(&p, &body).or_else(|| search_complaint(&body)) {
        return complaint;
    }
    if let Some(refusal) = change_refusal_for(&store, &index) {
        return refusal;
    }
    // a script says what to change; without one the walk rewrites each
    // document as it stands, which is what gives it a new version. One that
    // does not compile is refused before anything is read.
    if let Some(spec) = body.get("script") {
        match crate::painless::contexts::Compiled::of(spec, &|id| store.stored_script(id)) {
            Ok(_) => {}
            Err(e) if e.kind == "compile error" => return crate::api::compile_failure(e),
            Err(e) => return crate::api::script_failure(e),
        }
    }
    if let Some(refusal) = batch_refusal(&store, &index, batch_size(&p, &body)) {
        return refusal;
    }
    if let Some(failure) = too_few_copies(&store, &index, &p) {
        return unavailable(failure);
    }
    let proceed = body.get("conflicts").and_then(|v| v.as_str()) == Some("proceed")
        || p.get("conflicts").map(|v| v == "proceed").unwrap_or(false);
    let walk = Walk {
        kind: Kind::Update,
        batch: batch_size(&p, &body),
        search: body.clone(),
        read: index,
        body,
        store,
        p,
        proceed,
        caller: crate::security::layer::current_caller(),
    };
    launch(walk, &headers).await
}

pub async fn reindex(
    State(store): State<Store>,
    Query(p): Query<Params>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    if let Some(complaint) = reindex_complaint(&body) {
        return complaint;
    }
    if let Some(complaint) = complaint(&p, &body) {
        return complaint;
    }
    if let Some(spec) = body.get("script") {
        match crate::painless::contexts::Compiled::of(spec, &|id| store.stored_script(id)) {
            Ok(_) => {}
            Err(e) if e.kind == "compile error" => return crate::api::compile_failure(e),
            Err(e) => return crate::api::script_failure(e),
        }
    }
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
    // the indices are named in the body, where the security layer cannot see
    // them: a caller with the ordinary cluster permission for a composite
    // request could otherwise copy an index they may not read into one they
    // may, or write over an index they may not touch
    if remote.is_none()
        && let Some(why) = crate::security::item_refusal(
            &store,
            &["indices:data/read/search"],
            &crate::security::layer::indices_for_expr(&store, &from),
        )
    {
        return err(StatusCode::FORBIDDEN, "security_exception", why);
    }
    if let Some(why) = crate::security::item_refusal(
        &store,
        &["indices:data/write/index"],
        &crate::security::layer::indices_for_expr(&store, &to),
    ) {
        return err(StatusCode::FORBIDDEN, "security_exception", why);
    }
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
    // the destination is held still, the same way the by-query walks check
    // the index they change: without this every copy was refused and counted
    // as a version conflict, and the caller was told the documents were
    // already there
    if let Some(refused) = change_refusal_for(&store, &to) {
        return refused;
    }
    if remote.is_none()
        && let Some(refusal) = batch_refusal(&store, &from, batch_size(&p, &source))
    {
        return refusal;
    }
    // a destination that is not there yet is created, unless the cluster was
    // told which names may be created on the fly
    if store.get(&to).is_none()
        && let Some(complaint) = auto_create_complaint(&store, &to)
    {
        return complaint;
    }
    // with a script, the destination is made only when a document is
    // written to it: the script may send them all elsewhere, or drop them
    if body.get("script").is_none() && store.ensure(&to).is_err() {
        return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", "cannot open dest");
    }
    if let Some(failure) = too_few_copies(&store, &to, &p) {
        return unavailable(failure);
    }
    let proceed = body.get("conflicts").and_then(|v| v.as_str()) == Some("proceed");
    let walk = Walk {
        kind: Kind::Reindex,
        batch: batch_size(&p, &source),
        search: source,
        read: from,
        body,
        store,
        p,
        proceed,
        caller: crate::security::layer::current_caller(),
    };
    launch(walk, &headers).await
}

/// The answer to a walk that could not have the copies it asked for.
fn unavailable(failure: Value) -> Response {
    let figures = Figures { batches: 1, rate: f64::INFINITY, ..Default::default() };
    let mut answer = serde_json::Map::new();
    answer.insert("took".into(), json!(0));
    answer.insert("timed_out".into(), json!(false));
    answer.extend(figures.write(false));
    answer.insert("failures".into(), json!([failure]));
    (StatusCode::SERVICE_UNAVAILABLE, axum::Json(Value::Object(answer))).into_response()
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

/// Whether the document has been written to since the walk read it: the
/// sequence number it stands at now, if that is not the one the walk read.
fn written_since(g: &IdxState, seen: &Seen) -> Option<u64> {
    match (seen.seq_no, read_seq(g, &seen.id)) {
        (Some(saw), Some(now)) if saw != now => Some(now),
        _ => None,
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
