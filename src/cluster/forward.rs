//! A request lands on one node and belongs on another: writes go to the
//! node that holds the primary, changes to the cluster's metadata go to
//! the cluster manager, and a read is answered here when this node holds
//! an active copy of what it asks about and is sent to a node that does
//! otherwise. The request travels whole over the transport, with its
//! caller, and the answer comes back whole; the node that answers runs it
//! through its own router as if it had arrived there.
//!
//! The same layer scopes the replication buffer around a write: what the
//! handler wrote is copied to the replica copies before the answer goes
//! out, and the answer's `_shards` counts say how many copies took it.

use std::sync::OnceLock;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::replication;
use super::state::{ClusterState, ShardState};
use super::transport::{Envelope, Kind, NodeId};
use crate::security::Caller;
use crate::store::Store;

pub const FORWARD: &str = "internal:http/forward";

/// The router a forwarded request runs through on the node that answers it.
static ROUTER: OnceLock<axum::Router> = OnceLock::new();

/// The caller a forwarded request came with, set only by the transport
/// handler on this node, which is why the security layer trusts it.
#[derive(Clone, Debug)]
pub struct ForwardedCaller(pub Caller);

/// Where a request belongs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// answered by the node it arrived at
    Local,
    /// the cluster manager: metadata
    Manager,
    /// the node holding the primary of the named index (the manager for a
    /// bulk, which names its indices inside)
    Write(Option<String>),
    /// a node holding an active copy of every named index
    Read(String),
    /// every node holding an active copy of the named indices (all of them
    /// when none is named): a refresh reaches every copy in OpenSearch
    Broadcast(Option<String>),
}

fn first_segment(path: &str) -> (&str, &str) {
    let p = path.trim_start_matches('/');
    match p.find('/') {
        Some(i) => (&p[..i], &p[i + 1..]),
        None => (p, ""),
    }
}

/// Which node a request belongs on, from its method and path.
pub fn classify(method: &Method, path: &str) -> Target {
    let (head, rest) = first_segment(path);
    if head.is_empty() {
        return Target::Local;
    }
    let is_write = matches!(*method, Method::POST | Method::PUT | Method::DELETE | Method::PATCH);
    if head.starts_with('_') {
        return match head {
            "_bulk" => Target::Write(None),
            "_cluster" => match first_segment(rest).0 {
                "settings" | "voting_config_exclusions" if is_write => Target::Manager,
                "settings" => Target::Manager,
                _ => Target::Local,
            },
            "_template"
            | "_index_template"
            | "_component_template"
            | "_ingest"
            | "_scripts"
            | "_snapshot"
            | "_data_stream"
            | "_aliases"
            | "_alias"
            | "_rollover"
            | "_resolve"
            | "_reindex"
            | "_delete_by_query"
            | "_update_by_query" => Target::Manager,
            // a search is coordinated from the node it reached
            "_search" | "_msearch" | "_count" | "_search_shards" => Target::Local,
            // a task lives where the work ran: the manager, for the index
            // work that leaves a task behind
            "_tasks" => Target::Manager,
            "_mget" | "_field_caps" | "_validate" | "_mtermvectors" | "_rank_eval" | "_render"
            | "_pit" | "_stats" | "_segments" | "_recovery" | "_shard_stores" | "_mapping"
            | "_settings" | "_open" | "_close" => Target::Manager,
            "_refresh" | "_flush" | "_forcemerge" | "_cache" => Target::Broadcast(None),
            _ => Target::Local,
        };
    }
    // `/{index}` and `/{index}/...`
    let index = head.to_string();
    let (op, _) = first_segment(rest);
    if op.is_empty() {
        // the index itself: create, delete, get, exists
        return Target::Manager;
    }
    match op {
        "_doc" | "_create" | "_update" | "_bulk" if is_write => Target::Write(Some(index)),
        "_doc" | "_source" => {
            if *method == Method::GET || *method == Method::HEAD {
                Target::Read(index)
            } else {
                Target::Write(Some(index))
            }
        }
        "_update_by_query" | "_delete_by_query" => Target::Write(Some(index)),
        "_search" | "_count" | "_msearch" | "_search_shards" => Target::Local,
        "_refresh" | "_flush" | "_forcemerge" | "_cache" => Target::Broadcast(Some(index)),
        "_mget" | "_explain" | "_termvectors" | "_mtermvectors" | "_field_caps" | "_validate"
        | "_rank_eval" | "_analyze" | "_pit" => Target::Read(index),
        // the index's own metadata and maintenance: the node holding its primary
        "_settings"
        | "_mapping"
        | "_mappings"
        | "_alias"
        | "_aliases"
        | "_open"
        | "_close"
        | "_block"
        | "_stats"
        | "_segments"
        | "_recovery"
        | "_shard_stores"
        | "_upgrade"
        | "_split"
        | "_shrink"
        | "_clone"
        | "_rollover"
        | "_reload_search_analyzers" => Target::Write(Some(index)),
        _ => Target::Manager,
    }
}

/// The indices an expression names, among what the cluster knows.
fn resolve(state: &ClusterState, store: &Store, expr: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in expr.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()) {
        let neg = part.starts_with('-');
        let p = part.trim_start_matches('-');
        let names: Vec<String> = if p == "_all" || p == "*" {
            state.indices.keys().cloned().collect()
        } else if p.contains('*') {
            state.indices.keys().filter(|n| crate::store::glob_match(p, n)).cloned().collect()
        } else if state.indices.contains_key(p) {
            vec![p.to_string()]
        } else {
            // an alias: the store on this node knows what it points at
            store.resolve(p)
        };
        if neg {
            out.retain(|n| !names.contains(n));
        } else {
            for n in names {
                if !out.contains(&n) {
                    out.push(n);
                }
            }
        }
    }
    out
}

/// Whether this node holds an active copy of the index (a copy is a copy
/// of the index, whichever shard it is counted as).
fn held_here(state: &ClusterState, me: &NodeId, index: &str) -> bool {
    state.indices.contains_key(index)
        && state.routing.shards_of(index).any(|c| {
            c.node.as_ref() == Some(me)
                && matches!(c.state, ShardState::Started | ShardState::Relocating)
        })
}

/// The node holding the primary of an index's first shard.
fn primary_node(state: &ClusterState, index: &str) -> Option<NodeId> {
    state.routing.primary(index, 0).and_then(|p| p.node.clone())
}

/// A node holding active copies of all the indices, this node preferred,
/// then the one holding the most of them as primaries.
fn reader_for(state: &ClusterState, me: &NodeId, indices: &[String]) -> Option<NodeId> {
    if indices.iter().all(|i| held_here(state, me, i)) {
        return Some(me.clone());
    }
    let mut best: Option<(NodeId, usize)> = None;
    for n in state.nodes.keys() {
        let holds = indices.iter().filter(|i| held_here(state, n, i)).count();
        if holds == indices.len() {
            let primaries =
                indices.iter().filter(|i| primary_node(state, i).as_ref() == Some(n)).count();
            if best.as_ref().map(|(_, p)| primaries > *p).unwrap_or(true) {
                best = Some((n.clone(), primaries));
            }
        }
    }
    best.map(|(n, _)| n).or_else(|| state.cluster_manager.clone())
}

/// `wait_for_active_shards`: how many copies of each shard must be active
/// before a write may start; the plugin's refusal when they are not.
async fn active_shards_ok(p: &str, timeout_ms: u64, indices: &[String]) -> Option<Response> {
    let want_all = p == "all";
    let want: usize = if want_all { 0 } else { p.parse().unwrap_or(1) };
    if !want_all && want <= 1 {
        return None;
    }
    let started = std::time::Instant::now();
    loop {
        let short = super::with_state(|s| {
            for index in indices {
                let Some(m) = s.indices.get(index) else { continue };
                for shard in 0..m.number_of_shards {
                    let active = s
                        .routing
                        .shards_of(index)
                        .filter(|c| {
                            c.shard == shard
                                && matches!(c.state, ShardState::Started | ShardState::Relocating)
                        })
                        .count();
                    let need = if want_all { 1 + m.number_of_replicas as usize } else { want };
                    if active < need {
                        return Some((index.clone(), shard, active, need));
                    }
                }
            }
            None
        });
        let Some((index, shard, have, need)) = short else { return None };
        if started.elapsed().as_millis() as u64 >= timeout_ms {
            let reason = format!(
                "[{index}][{shard}] Not enough active copies to meet shard count of [{}] (have {have}, needed {need}). Timeout: [{}]",
                if want_all { "ALL".to_string() } else { need.to_string() },
                time_text(timeout_ms)
            );
            return Some(crate::api::err(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable_shards_exception",
                &reason,
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// The query string as a map; a flag without a value reads `true`.
fn parse_query(q: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for pair in q.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, "true"),
        };
        let decode = |s: &str| -> String {
            let mut bytes = Vec::with_capacity(s.len());
            let b = s.as_bytes();
            let mut i = 0;
            while i < b.len() {
                match b[i] {
                    b'%' if i + 2 < b.len() + 1 && i + 2 <= b.len() - 1 + 1 => {
                        if let Ok(h) = u8::from_str_radix(&s[i + 1..(i + 3).min(s.len())], 16) {
                            bytes.push(h);
                            i += 3;
                            continue;
                        }
                        bytes.push(b'%');
                        i += 1;
                    }
                    b'+' => {
                        bytes.push(b' ');
                        i += 1;
                    }
                    c => {
                        bytes.push(c);
                        i += 1;
                    }
                }
            }
            String::from_utf8_lossy(&bytes).into_owned()
        };
        out.insert(decode(k), decode(v));
    }
    out
}

/// A time in the form the plugin prints it in messages: `1m`, `30s`, `500ms`.
fn time_text(ms: u64) -> String {
    if ms % 60_000 == 0 && ms > 0 {
        format!("{}m", ms / 60_000)
    } else if ms % 1_000 == 0 && ms > 0 {
        format!("{}s", ms / 1_000)
    } else {
        format!("{ms}ms")
    }
}

/// The layer: answer here, or send the request where it belongs.
pub async fn layer(State(store): State<Store>, req: Request, next: Next) -> Response {
    // a request another node sent here is answered here
    if req.extensions().get::<ForwardedCaller>().is_some() {
        return run_with_replication(&store, req, next).await;
    }
    let Some(rt) = super::runtime() else {
        return run_with_replication(&store, req, next).await;
    };
    let me = rt.local();
    let path = req.uri().path().to_string();
    let query = parse_query(req.uri().query().unwrap_or(""));
    let target = classify(req.method(), &path);
    if let Target::Broadcast(expr) = &target {
        return broadcast(&rt, &store, expr.as_deref(), req, next).await;
    }
    // what this request will have changed about the cluster's indices, so the
    // answer can wait until this node knows it: a create that answered before
    // the state reached the node the client is talking to would have the very
    // next request say the index is not there
    let settles = settles_metadata(req.method(), &path);
    let (to, write_indices): (Option<NodeId>, Vec<String>) = super::with_state(|s| {
        // nothing committed yet: one node, everything local
        if s.version == 0 || s.nodes.len() <= 1 {
            return (None, Vec::new());
        }
        match &target {
            Target::Local | Target::Broadcast(_) => (None, Vec::new()),
            Target::Manager => (s.cluster_manager.clone().filter(|m| *m != me), Vec::new()),
            Target::Write(None) => (
                s.cluster_manager.clone().filter(|m| *m != me),
                s.indices.keys().cloned().collect(),
            ),
            Target::Write(Some(index)) => {
                let names = resolve(s, &store, index);
                let node = names
                    .first()
                    .and_then(|n| primary_node(s, n))
                    .or_else(|| s.cluster_manager.clone());
                (node.filter(|n| *n != me), names)
            }
            Target::Read(expr) => {
                let names = resolve(s, &store, expr);
                if names.is_empty() {
                    return (None, Vec::new());
                }
                (reader_for(s, &me, &names).filter(|n| *n != me), Vec::new())
            }
        }
    });
    // a write waits for the copies it was told to wait for
    if matches!(target, Target::Write(_)) {
        if let Some(w) = query.get("wait_for_active_shards") {
            let timeout =
                query.get("timeout").and_then(|t| super::allocation::time_ms(t)).unwrap_or(60_000);
            if let Some(r) = active_shards_ok(w, timeout, &write_indices).await {
                return r;
            }
        }
    }
    let Some(to) = to else {
        let r = run_with_replication(&store, req, next).await;
        return wait_for_metadata(&store, settles, r).await;
    };
    let r = forward(&rt, &to, req).await;
    wait_for_metadata(&store, settles, r).await
}

/// The index a request makes or unmakes, and which of the two it is.
fn settles_metadata(method: &Method, path: &str) -> Option<(String, bool)> {
    let (index, rest) = first_segment(path);
    if index.is_empty() || index.starts_with('_') {
        return None;
    }
    let (op, _) = first_segment(rest);
    match (method, op) {
        (&Method::PUT, "") | (&Method::POST, "") => Some((index.to_string(), true)),
        (&Method::DELETE, "") => Some((index.to_string(), false)),
        // a write makes the index it names when there is none
        (&Method::PUT | &Method::POST, "_doc" | "_create" | "_update") => {
            Some((index.to_string(), true))
        }
        _ => None,
    }
}

/// Hold the answer until this node knows what the cluster now holds.
///
/// The cluster manager makes an index and publishes it; the node the client
/// is talking to hears a moment later. OpenSearch answers `acknowledged` when
/// every node has the state, and the request after a create finds the index
/// wherever it is sent, so this one waits too -- briefly, and only for the
/// node that answers.
async fn wait_for_metadata(
    store: &Store,
    settles: Option<(String, bool)>,
    response: Response,
) -> Response {
    let Some((index, present)) = settles else { return response };
    if !response.status().is_success() {
        return response;
    }
    // the name may be an alias, or a data stream's backing index
    let known = |store: &Store| {
        !store.resolve(&index).is_empty()
            || super::with_state(|s| {
                s.indices.contains_key(&index)
                    || s.indices.values().any(|m| m.aliases.get(&index).is_some())
            })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while known(store) != present && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    response
}

/// A request every copy answers: run here for the copies here, forwarded
/// to every other node holding one, the shard tallies added up. A node
/// that does not answer counts its copies as failed, the way a shard a
/// refresh could not reach is failed in OpenSearch.
async fn broadcast(
    rt: &std::sync::Arc<super::runtime::Runtime>,
    store: &Store,
    expr: Option<&str>,
    req: Request,
    next: Next,
) -> Response {
    let me = rt.local();
    let mut here_copies = 0usize;
    let (here, others): (bool, Vec<(NodeId, usize)>) = super::with_state(|s| {
        if s.version == 0 || s.nodes.len() <= 1 {
            return (true, Vec::new());
        }
        let names: Vec<String> = match expr {
            Some(e) => resolve(s, store, e),
            None => s.indices.keys().cloned().collect(),
        };
        let mut copies: std::collections::BTreeMap<NodeId, usize> = Default::default();
        for n in &names {
            for c in s.routing.shards_of(n) {
                if let Some(node) = &c.node {
                    if matches!(c.state, ShardState::Started | ShardState::Relocating) {
                        *copies.entry(node.clone()).or_default() += 1;
                    }
                }
            }
        }
        let here = copies.contains_key(&me) || names.iter().any(|n| store.get(n).is_some());
        here_copies = copies.get(&me).copied().unwrap_or(0);
        (here, copies.into_iter().filter(|(n, _)| *n != me).collect())
    });
    if others.is_empty() {
        return run_with_replication(store, req, next).await;
    }
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, crate::api::max_content_bytes() as usize).await {
        Ok(b) => b,
        Err(_) => {
            return crate::api::err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                "body could not be read",
            );
        }
    };
    let rebuild = |parts: &axum::http::request::Parts| -> Request {
        let mut b =
            axum::http::Request::builder().method(parts.method.clone()).uri(parts.uri.clone());
        for (k, v) in parts.headers.iter() {
            b = b.header(k, v);
        }
        let mut r = b.body(Body::from(bytes.clone())).unwrap_or_default();
        *r.extensions_mut() = parts.extensions.clone();
        r
    };
    // the other holders, all at once; the local run in this task
    let mut waits = Vec::new();
    for (node, n) in &others {
        let r = rebuild(&parts);
        let rt = rt.clone();
        let node = node.clone();
        let n = *n;
        waits.push(tokio::spawn(async move {
            let answer = forward(&rt, &node, r).await;
            (n, answer)
        }));
    }
    let local: Option<Response> =
        if here { Some(run_with_replication(store, rebuild(&parts), next).await) } else { None };
    let mut answers: Vec<(usize, Response)> = Vec::new();
    for w in waits {
        if let Ok(a) = w.await {
            answers.push(a);
        }
    }
    // the answer's body is the local one, or the first that came back, with
    // the tallies of every copy added into `_shards`
    let mut total = 0u64;
    let mut successful = 0u64;
    let mut failed = 0u64;
    let mut first: Option<(StatusCode, Value)> = None;
    let mut fold = |copies: usize, r: Response| {
        let status = r.status();
        let body = r.into_body();
        (copies, status, body)
    };
    let mut parts_in: Vec<(usize, StatusCode, Body)> = Vec::new();
    if let Some(l) = local {
        parts_in.push(fold(here_copies, l));
    }
    for (n, r) in answers {
        parts_in.push(fold(n, r));
    }
    for (copies, status, body) in parts_in {
        let bytes = axum::body::to_bytes(body, crate::api::max_content_bytes() as usize)
            .await
            .unwrap_or_default();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        // the copies each node answered for, not the tallies it reported: a
        // node counts the shards it knows of, and adding those up would count
        // one shard once per node
        if status.is_success() {
            successful += copies as u64;
        } else {
            failed += copies as u64;
        }
        total += copies as u64;
        if first.is_none()
            || (!first.as_ref().map(|(s, _)| s.is_success()).unwrap_or(false)
                && status.is_success())
        {
            first = Some((status, v));
        }
    }
    let Some((status, mut v)) = first else {
        return crate::api::err(
            StatusCode::SERVICE_UNAVAILABLE,
            "node_not_connected_exception",
            "no node answered",
        );
    };
    if status.is_success() && v.get("_shards").is_some() {
        v["_shards"] = json!({"total": total, "successful": successful, "failed": failed});
    }
    let mut r = Response::new(Body::from(serde_json::to_vec(&v).unwrap_or_default()));
    *r.status_mut() = status;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

/// Run the handler here, then copy what it wrote to the replica copies and
/// say in the answer how many took it.
async fn run_with_replication(store: &Store, req: Request, next: Next) -> Response {
    let refresh =
        parse_query(req.uri().query().unwrap_or("")).get("refresh").cloned().unwrap_or_default();
    let _ = store;
    replication::WRITES
        .scope(std::cell::RefCell::new(Vec::new()), async move {
            let response = next.run(req).await;
            let ops = replication::WRITES.with(|w| std::mem::take(&mut *w.borrow_mut()));
            if ops.is_empty() {
                return response;
            }
            replication::finish(response, ops, &refresh).await
        })
        .await
}

/// Send the request to the node, whole, and hand back its answer, whole.
async fn forward(rt: &super::runtime::Runtime, to: &NodeId, req: Request) -> Response {
    let caller = crate::security::layer::current_caller().unwrap_or_default();
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, crate::api::max_content_bytes() as usize).await {
        Ok(b) => b,
        Err(_) => {
            return crate::api::err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                "body could not be read",
            );
        }
    };
    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .filter(|(k, _)| {
            matches!(k.as_str(), "content-type" | "accept" | "x-opaque-id" | "content-encoding")
        })
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string())))
        .collect();
    let ask = json!({
        "method": parts.method.as_str(),
        "uri": parts.uri.to_string(),
        "headers": headers,
        "body": String::from_utf8_lossy(&bytes),
        "caller": caller,
    });
    let answer = rt
        .call(
            to,
            FORWARD,
            serde_json::to_vec(&ask).unwrap_or_default(),
            std::time::Duration::from_secs(120),
        )
        .await;
    let Some(answer) = answer else {
        return crate::api::err(
            StatusCode::SERVICE_UNAVAILABLE,
            "node_not_connected_exception",
            &format!("[{}] Node not connected", to.as_str()),
        );
    };
    if answer.kind == Kind::Error {
        return crate::api::err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "exception",
            String::from_utf8_lossy(&answer.body).into_owned(),
        );
    }
    let v: Value = serde_json::from_slice(&answer.body).unwrap_or(Value::Null);
    let status = v.get("status").and_then(|s| s.as_u64()).unwrap_or(500) as u16;
    let mut r =
        Response::new(Body::from(v.get("body").and_then(|b| b.as_str()).unwrap_or("").to_string()));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if let Some(hs) = v.get("headers").and_then(|h| h.as_array()) {
        for h in hs {
            if let (Some(k), Some(val)) =
                (h.get(0).and_then(|x| x.as_str()), h.get(1).and_then(|x| x.as_str()))
            {
                if let (Ok(name), Ok(value)) =
                    (header::HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(val))
                {
                    r.headers_mut().append(name, value);
                }
            }
        }
    }
    r
}

/// Answer a forwarded request: through this node's own router, as its caller.
pub fn install(router: axum::Router) {
    let _ = ROUTER.set(router);
    let Some(rt) = super::runtime() else { return };
    rt.register(
        FORWARD,
        std::sync::Arc::new(|e: Envelope| -> super::runtime::DataFuture {
            Box::pin(async move {
                let from = super::identity().id.clone();
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let caller: Caller = v
                    .get("caller")
                    .and_then(|c| serde_json::from_value(c.clone()).ok())
                    .unwrap_or_default();
                let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("GET");
                let uri = v.get("uri").and_then(|u| u.as_str()).unwrap_or("/");
                let body = v.get("body").and_then(|b| b.as_str()).unwrap_or("").to_string();
                let Some(router) = ROUTER.get().cloned() else {
                    return e.error(from, "no router on this node");
                };
                let mut builder = axum::http::Request::builder().method(method).uri(uri);
                if let Some(hs) = v.get("headers").and_then(|h| h.as_array()) {
                    for h in hs {
                        if let (Some(k), Some(val)) =
                            (h.get(0).and_then(|x| x.as_str()), h.get(1).and_then(|x| x.as_str()))
                        {
                            builder = builder.header(k, val);
                        }
                    }
                }
                let Ok(mut req) = builder.body(Body::from(body)) else {
                    return e.error(from, "the forwarded request could not be rebuilt");
                };
                req.extensions_mut().insert(ForwardedCaller(caller));
                use tower::ServiceExt;
                let response = match router.oneshot(req).await {
                    Ok(r) => r,
                    Err(never) => match never {},
                };
                let status = response.status().as_u16();
                let headers: Vec<(String, String)> = response
                    .headers()
                    .iter()
                    .filter_map(|(k, v)| {
                        v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string()))
                    })
                    .filter(|(k, _)| k != "content-length" && k != "transfer-encoding")
                    .collect();
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default();
                let out = json!({
                    "status": status,
                    "headers": headers,
                    "body": String::from_utf8_lossy(&bytes),
                });
                e.response(from, serde_json::to_vec(&out).unwrap_or_default())
            })
        }),
    );
}

#[allow(dead_code)]
fn _unused(_: &dyn IntoResponse) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_go_where_they_belong() {
        let get = Method::GET;
        let post = Method::POST;
        let put = Method::PUT;
        let del = Method::DELETE;
        assert_eq!(classify(&get, "/"), Target::Local);
        assert_eq!(classify(&get, "/_cluster/health"), Target::Local);
        assert_eq!(classify(&get, "/_cat/shards"), Target::Local);
        assert_eq!(classify(&put, "/_cluster/settings"), Target::Manager);
        assert_eq!(classify(&put, "/logs"), Target::Manager);
        assert_eq!(classify(&del, "/logs"), Target::Manager);
        assert_eq!(classify(&put, "/logs/_mapping"), Target::Write(Some("logs".into())));
        assert_eq!(classify(&get, "/logs/_settings"), Target::Write(Some("logs".into())));
        assert_eq!(classify(&put, "/logs/_doc/1"), Target::Write(Some("logs".into())));
        assert_eq!(classify(&post, "/logs/_update/1"), Target::Write(Some("logs".into())));
        assert_eq!(classify(&del, "/logs/_doc/1"), Target::Write(Some("logs".into())));
        assert_eq!(classify(&post, "/_bulk"), Target::Write(None));
        assert_eq!(classify(&post, "/logs/_bulk"), Target::Write(Some("logs".into())));
        assert_eq!(classify(&get, "/logs/_doc/1"), Target::Read("logs".into()));
        assert_eq!(classify(&get, "/logs,metrics/_search"), Target::Local);
        assert_eq!(classify(&post, "/logs/_search"), Target::Local);
        assert_eq!(classify(&post, "/_search"), Target::Local);
        assert_eq!(classify(&post, "/_count"), Target::Local);
        // a refresh reaches every copy, not just the primary's node
        assert_eq!(classify(&post, "/logs/_refresh"), Target::Broadcast(Some("logs".into())));
        assert_eq!(classify(&post, "/_refresh"), Target::Broadcast(None));
        assert_eq!(classify(&post, "/logs/_flush"), Target::Broadcast(Some("logs".into())));
        assert_eq!(classify(&get, "/logs/_explain/1"), Target::Read("logs".into()));
        assert_eq!(classify(&post, "/_search/scroll"), Target::Local);
        assert_eq!(classify(&put, "/_ingest/pipeline/p"), Target::Manager);
        assert_eq!(classify(&get, "/_nodes/stats"), Target::Local);
    }
}
