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

tokio::task_local! {
    /// Set while this node answers a request another node sent it.
    static ANSWERING_FORWARD: bool;
}

/// Is this node answering for another node's request? A listing every node
/// contributes rows to asks, so that the rows no node holds are written once.
pub fn answering_forward() -> bool {
    ANSWERING_FORWARD.try_with(|v| *v).unwrap_or(false)
}

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
    // `_all` names every index rather than an endpoint of its own
    if head == "_all" {
        return classify_index(method, "_all", rest, is_write);
    }
    if head.starts_with('_') {
        return match head {
            "_bulk" => Target::Write(None),
            // the tables that count what a node holds are asked of every node
            "_stats" => Target::Broadcast(None),
            "_cat" => match first_segment(rest).0 {
                // the tables whose numbers a node can only give for what it
                // holds: each node answers for its own copies
                "segments" | "fielddata" | "indices" | "shards" => {
                    let (_, after) = first_segment(rest);
                    let name = first_segment(after).0;
                    Target::Broadcast((!name.is_empty()).then(|| name.to_string()))
                }
                _ => Target::Local,
            },
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
    classify_index(method, head, rest, is_write)
}

/// Which node a request about one index (or an expression naming several)
/// belongs on.
fn classify_index(method: &Method, head: &str, rest: &str, is_write: bool) -> Target {
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
        // opening and closing an index is every holder's work: the copies are
        // what a search reads, and the node holding the primary is the one
        // whose metadata says the index is closed
        "_refresh" | "_flush" | "_forcemerge" | "_cache" | "_open" | "_close" => {
            Target::Broadcast(Some(index))
        }
        // what an index cost in memory and on disk is what every node holding
        // a copy of it spent, added up
        "_stats" => Target::Broadcast(Some(index)),
        "_mget" | "_explain" | "_termvectors" | "_mtermvectors" | "_field_caps" | "_validate"
        | "_rank_eval" | "_analyze" | "_pit" => Target::Read(index),
        // the index's own metadata and maintenance: the node holding its primary
        "_settings"
        | "_mapping"
        | "_mappings"
        | "_alias"
        | "_aliases"
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
            // an alias: the cluster's metadata says what it points at,
            // wherever those indices are held
            let by_alias: Vec<String> = state
                .indices
                .iter()
                .filter(|(_, m)| m.aliases.get(p).is_some())
                .map(|(n, _)| n.clone())
                .collect();
            if by_alias.is_empty() { store.resolve(p) } else { by_alias }
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
        return ANSWERING_FORWARD.scope(true, run_with_replication(&store, req, next)).await;
    }
    let Some(rt) = super::runtime() else {
        return run_with_replication(&store, req, next).await;
    };
    let me = rt.local();
    let path = req.uri().path().to_string();
    let query = parse_query(req.uri().query().unwrap_or(""));
    let target = classify(req.method(), &path);
    if let Target::Broadcast(expr) = &target {
        // closing or opening an index changes what the cluster says about it,
        // and the answer waits for that to be published: the request after it
        // asks about a closed index and must be told it is closed
        let closing = path.ends_with("/_close").then_some(true);
        let opening = path.ends_with("/_open").then_some(false);
        let want_closed = closing.or(opening);
        let named = expr.clone();
        let r = broadcast(&rt, &store, expr.as_deref(), req, next).await;
        if let (Some(closed), Some(e)) = (want_closed, named) {
            if r.status().is_success() {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                loop {
                    let settled = super::with_state(|s| {
                        let names = resolve(s, &store, &e);
                        !names.is_empty()
                            && names.iter().all(|n| {
                                s.indices
                                    .get(n)
                                    .map(|m| (m.state == "close") == closed)
                                    .unwrap_or(true)
                            })
                    });
                    if settled || std::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            }
        }
        return r;
    }
    // what this request will have changed about the cluster's indices, so the
    // answer can wait until this node knows it: a create that answered before
    // the state reached the node the client is talking to would have the very
    // next request say the index is not there
    // A bulk is coordinated, not run wherever it lands: each operation
    // belongs to an index, and an index's writes belong to the node holding
    // its primary. Running the whole body on the node the request reached
    // would have a copy take writes the primary never saw.
    if matches!(target, Target::Write(None)) && super::with_state(|s| s.nodes.len() > 1) {
        return coordinate_bulk(&rt, &store, req, next).await;
    }
    let settles = settles_metadata(req.method(), &path);
    let settles_c = settles_custom(req.method(), &path);
    let settles_a = settles_alias(req.method(), &path);
    let version_before = super::with_state(|s| s.version);
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
        let r = wait_for_metadata(&store, settles, r).await;
        let r = wait_for_custom(settles_c, r).await;
        return wait_for_alias(&store, settles_a, version_before, r).await;
    };
    let r = forward(&rt, &to, req).await;
    let r = wait_for_metadata(&store, settles, r).await;
    let r = wait_for_custom(settles_c, r).await;
    wait_for_alias(&store, settles_a, version_before, r).await
}

/// Hold the answer until the alias the request made is one the cluster knows:
/// an alias belongs to an index, and the index may be held elsewhere.
async fn wait_for_alias(
    store: &Store,
    settles: Option<(String, String, bool)>,
    version_before: u64,
    response: Response,
) -> Response {
    let Some((index, alias, present)) = settles else { return response };
    if !response.status().is_success() {
        return response;
    }
    // a request whose body named the indices can only wait for the cluster to
    // publish something, so it waits briefly rather than the full ten seconds
    let wait = if alias.is_empty() { 2 } else { 5 };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait);
    loop {
        let settled = super::with_state(|s| {
            if alias.is_empty() {
                // the body named the indices: something was published, which
                // is as much as this can wait for
                return s.version > version_before;
            }
            let names = resolve(s, store, &index);
            !names.is_empty()
                && names.iter().all(|n| {
                    s.indices
                        .get(n)
                        .map(|m| m.aliases.get(&alias).is_some() == present)
                        .unwrap_or(!present)
                })
        });
        if settled || std::time::Instant::now() >= deadline {
            return response;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Hold the answer until this node has the template (or pipeline, or script)
/// the cluster now holds: the request after a template is made looks it up,
/// and on a cluster it is the manager that keeps it and publishes it.
async fn wait_for_custom(
    settles: Option<(&'static str, String, bool)>,
    response: Response,
) -> Response {
    let Some((section, name, present)) = settles else { return response };
    if !response.status().is_success() {
        return response;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let there = super::with_state(|s| {
            let c = &s.customs;
            match section {
                "pipelines" => c
                    .get("pipelines")
                    .map(|p| {
                        p.get("ingest").and_then(|i| i.get(&name)).is_some()
                            || p.get("search").and_then(|i| i.get(&name)).is_some()
                    })
                    .unwrap_or(false),
                other => c.get(other).and_then(|o| o.get(&name)).is_some(),
            }
        });
        if there == present || std::time::Instant::now() >= deadline {
            return response;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// The alias a request makes or unmakes, as (index expression, alias, there
/// afterwards). `POST /_aliases` names its indices in the body, so it asks
/// only that the cluster publish something after it.
fn settles_alias(method: &Method, path: &str) -> Option<(String, String, bool)> {
    let (head, rest) = first_segment(path);
    let present = match *method {
        Method::PUT | Method::POST => true,
        Method::DELETE => false,
        _ => return None,
    };
    if head == "_aliases" {
        return Some((String::new(), String::new(), present));
    }
    if head.is_empty() || head.starts_with('_') {
        return None;
    }
    let (op, after) = first_segment(rest);
    if op != "_alias" && op != "_aliases" {
        return None;
    }
    let name = first_segment(after).0;
    if name.is_empty() || name.contains('*') {
        return None;
    }
    Some((head.to_string(), name.to_string(), present))
}

/// The template, pipeline or script a request makes or unmakes: which part
/// of the cluster's customs it lands in, its name, and whether it should be
/// there afterwards.
fn settles_custom(method: &Method, path: &str) -> Option<(&'static str, String, bool)> {
    let (head, rest) = first_segment(path);
    let present = match *method {
        Method::PUT | Method::POST => true,
        Method::DELETE => false,
        _ => return None,
    };
    let (section, name) = match head {
        "_template" => ("templates", first_segment(rest).0),
        "_component_template" => ("components", first_segment(rest).0),
        "_index_template" => ("templates", first_segment(rest).0),
        "_scripts" => ("scripts", first_segment(rest).0),
        "_ingest" => {
            let (kind, after) = first_segment(rest);
            if kind != "pipeline" {
                return None;
            }
            ("pipelines", first_segment(after).0)
        }
        _ => return None,
    };
    if name.is_empty() || name.contains('*') {
        return None;
    }
    Some((section, name.to_string(), present))
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
    // On a cluster it is the published state that counts, routing and all:
    // the manager makes the index in its own store first, and an answer given
    // on the strength of that would have the next request find the shards
    // unplaced. The name may be an alias, or a data stream's backing index.
    let clustered = super::runtime().map(|rt| rt.state().nodes.len() > 1).unwrap_or(false);
    let known = |store: &Store| {
        let published = super::with_state(|s| {
            // a delete may name a pattern, and is done when nothing it
            // reaches is left -- in the metadata or in the routing, which is
            // what a listing counts
            let matches =
                |n: &String| index == "*" || index == "_all" || crate::store::glob_match(&index, n);
            let named = if index.contains('*') || index == "_all" {
                s.indices.keys().any(matches) || s.routing.indices.keys().any(matches)
            } else {
                s.indices.contains_key(&index)
                    || s.indices.values().any(|m| m.aliases.get(&index).is_some())
                    || (!present && s.routing.indices.contains_key(&index))
            };
            named
        });
        if clustered { published } else { published || !store.resolve(&index).is_empty() }
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
    let path_is_stats = req.uri().path().contains("/_stats") || req.uri().path() == "/_stats";
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
    // a `cat` table always runs here as well: the node that took the request
    // is the one that speaks for the copies no node holds
    let path_is_cat = parts.uri.path().starts_with("/_cat/");
    let local: Option<Response> = if here || path_is_cat {
        Some(run_with_replication(store, rebuild(&parts), next).await)
    } else {
        None
    };
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
    let mut per_index: std::collections::BTreeMap<String, Value> = Default::default();
    let mut lines: Vec<String> = Vec::new();
    let mut cat_json: Vec<Value> = Vec::new();
    let path_is_cat_body = parts.uri.path().starts_with("/_cat/");
    let path_is_cat = path_is_cat_body;
    // counters -- what an index cost -- are added up over the nodes holding it
    let summing = path_is_stats;
    let mut summed: Option<Value> = None;
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
        if v.is_null() && !bytes.is_empty() {
            // a table rather than an answer of fields: the rows of every node,
            // with one header between them
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if status.is_success() {
                lines.push(text);
            }
        }
        // the copies each node answered for, not the tallies it reported: a
        // node counts the shards it knows of, and adding those up would count
        // one shard once per node
        if status.is_success() {
            successful += copies as u64;
        } else {
            failed += copies as u64;
        }
        total += copies as u64;
        if summing {
            match summed.as_mut() {
                Some(acc) => add_into(acc, &v),
                None => summed = Some(v.clone()),
            }
        }
        // a `cat` table asked for as json: every node's rows, in one array
        if path_is_cat && v.is_array() && status.is_success() {
            if let Some(a) = v.as_array() {
                cat_json.extend(a.iter().cloned());
            }
            continue;
        }
        if let Some(idx) = v.get("indices").and_then(|i| i.as_object()) {
            for (k, val) in idx {
                per_index.insert(k.clone(), val.clone());
            }
        }
        if first.is_none()
            || (!first.as_ref().map(|(s, _)| s.is_success()).unwrap_or(false)
                && status.is_success())
        {
            first = Some((status, v));
        }
    }
    if path_is_cat && !cat_json.is_empty() {
        let query = parse_query(parts.uri.query().unwrap_or(""));
        sort_cat_json(&mut cat_json, query.get("s").map(|x| x.as_str()));
        let mut r = Response::new(Body::from(Value::Array(cat_json).to_string()));
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        return r;
    }
    if !lines.is_empty() {
        // One header, then every node's rows. Every node ran the same handler
        // with the same parameters, so either all the tables carry a header
        // line or none of them do; the header kept is the one from a node
        // that had rows to describe, since a node with nothing to say cannot
        // know which columns the rows will need.
        let query = parse_query(parts.uri.query().unwrap_or(""));
        let headed =
            query.contains_key("v") && query.get("v").map(|v| v != "false").unwrap_or(true);
        let mut header: Option<String> = None;
        let mut rows: Vec<String> = Vec::new();
        for table in &lines {
            let mut ls = table.lines();
            if headed {
                let Some(head) = ls.next() else { continue };
                let body: Vec<String> = ls.map(|l| l.to_string()).collect();
                if header.is_none() || (!body.is_empty() && rows.is_empty()) {
                    header = Some(head.to_string());
                }
                rows.extend(body);
            } else {
                rows.extend(ls.map(|l| l.to_string()));
            }
        }
        let mut body = String::new();
        if let Some(h) = header {
            body.push_str(&h);
            body.push('\n');
        }
        for r in rows.iter().filter(|r| !r.trim().is_empty()) {
            body.push_str(r);
            body.push('\n');
        }
        let mut r = Response::new(Body::from(body));
        r.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=UTF-8"));
        return r;
    }
    let Some((status, mut v)) = first else {
        return crate::api::err(
            StatusCode::SERVICE_UNAVAILABLE,
            "node_not_connected_exception",
            "no node answered",
        );
    };
    // the summed counters first, then the shard tallies the routing knows
    if summing && status.is_success() {
        if let Some(t) = summed.take() {
            v = t;
        }
    }
    if status.is_success() && v.get("_shards").is_some() {
        if summing {
            // what an index asked for, the way OpenSearch counts a stats
            // answer: every shard and every replica of it, held or not
            let asked: u64 = super::with_state(|s| {
                let names: Vec<String> = match expr {
                    Some(e) => resolve(s, store, e),
                    None => s.indices.keys().cloned().collect(),
                };
                names
                    .iter()
                    .filter_map(|n| s.indices.get(n))
                    .map(|m| m.number_of_shards as u64 * (1 + m.number_of_replicas as u64))
                    .sum()
            });
            v["_shards"] = json!({"total": asked, "successful": asked, "failed": 0});
        } else {
            v["_shards"] = json!({"total": total, "successful": successful, "failed": failed});
        }
    }
    // an answer that speaks per index -- a close, an open -- speaks for the
    // indices every node answered for
    // (a summed answer already carries every node's indices, added up)
    if status.is_success() && !summing && !per_index.is_empty() {
        v["indices"] = Value::Object(per_index.into_iter().collect());
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



/// Put a `cat` table's rows in the order the request asked for, the way one
/// node would have ordered them before its rows were joined with the others'.
fn sort_cat_json(rows: &mut [Value], spec: Option<&str>) {
    let keys: Vec<(String, bool)> = match spec.filter(|s| !s.is_empty()) {
        Some(s) => s
            .split(',')
            .map(|k| match k.trim().split_once(':') {
                Some((name, dir)) => (name.to_string(), dir.eq_ignore_ascii_case("desc")),
                None => (k.trim().to_string(), false),
            })
            .collect(),
        // no order asked for: by index name, which is how each node had
        // already sorted its own rows
        None => rows
            .first()
            .and_then(|r| r.as_object())
            .and_then(|o| {
                o.keys().find(|k| *k == "index").or_else(|| o.keys().next()).cloned()
            })
            .map(|k| vec![(k, false)])
            .unwrap_or_default(),
    };
    if keys.is_empty() {
        return;
    }
    rows.sort_by(|a, b| {
        for (name, desc) in &keys {
            let pick = |v: &Value| -> String {
                v.as_object()
                    .and_then(|o| {
                        o.iter()
                            .find(|(k, _)| crate::api::cat::cat_column_matches(k, name))
                            .map(|(_, v)| v)
                    })
                    .and_then(|v| match v {
                        Value::String(s) => Some(s.clone()),
                        other => Some(other.to_string()),
                    })
                    .unwrap_or_default()
            };
            let ord = pick(a).cmp(&pick(b));
            let ord = if *desc { ord.reverse() } else { ord };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
}

/// Split a bulk body by the node that holds each index's primary, send each
/// part to that node, and put the answers back in the order they were asked.
///
/// An index the cluster does not know yet is the manager's to make, so its
/// operations go there; the operations for indices held here run here.
async fn coordinate_bulk(
    rt: &std::sync::Arc<super::runtime::Runtime>,
    store: &Store,
    req: Request,
    next: Next,
) -> Response {
    let me = rt.local();
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
    let text = String::from_utf8_lossy(&bytes).into_owned();
    // the path may name the index every operation defaults to
    let default_index = {
        let p = parts.uri.path();
        let p = p.strip_prefix('/').unwrap_or(p);
        match p.strip_suffix("_bulk").and_then(|x| x.strip_suffix('/')) {
            Some(i) if !i.is_empty() => Some(i.to_string()),
            _ => None,
        }
    };
    // each operation, as the lines it is written on and the index it names
    let mut groups: std::collections::BTreeMap<Option<NodeId>, (Vec<usize>, String)> =
        Default::default();
    let mut count = 0usize;
    // the operations, each as the lines it was written on and the index it
    // names; the body is read through before anything is sent
    enum Split {
        Ops(Vec<(String, Option<String>, String)>),
        Unreadable,
    }
    let split = {
        let mut out: Vec<(String, Option<String>, String)> = Vec::new();
        let mut bad = false;
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        while let Some(action_line) = lines.next() {
            let Ok(action) = serde_json::from_str::<Value>(action_line) else {
                bad = true;
                break;
            };
            let Some((op, meta)) = action.as_object().and_then(|o| o.iter().next()) else {
                bad = true;
                break;
            };
            let index = meta
                .get("_index")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| default_index.clone());
            let doc_line = if op == "delete" { None } else { lines.next().map(|l| l.to_string()) };
            let Some(index) = index else {
                bad = true;
                break;
            };
            out.push((action_line.to_string(), doc_line, index));
        }
        if bad { Split::Unreadable } else { Split::Ops(out) }
    };
    // a body this node cannot read is the handler's complaint to make
    let Split::Ops(ops) = split else {
        return run_local_bulk(store, parts, bytes, next).await;
    };
    // an index a write is about to make is not in the cluster's metadata yet;
    // the answer waits until this node knows it, the way it does for a create
    let mut made: Vec<String> = Vec::new();
    for (action_line, doc_line, index) in ops {
        // an alias or a data stream names an index; an unknown name is the
        // manager's to create
        if super::with_state(|s| resolve(s, store, &index).is_empty()) && !made.contains(&index) {
            made.push(index.clone());
        }
        let node = super::with_state(|s| {
            let names = resolve(s, store, &index);
            match names.first() {
                Some(n) => primary_node(s, n).or_else(|| s.cluster_manager.clone()),
                None => s.cluster_manager.clone(),
            }
        });
        let entry = groups.entry(node).or_insert_with(|| (Vec::new(), String::new()));
        entry.0.push(count);
        entry.1.push_str(&action_line);
        entry.1.push('\n');
        if let Some(d) = doc_line {
            entry.1.push_str(&d);
            entry.1.push('\n');
        }
        count += 1;
    }
    // everything belongs to one node: the request goes there whole
    if groups.len() <= 1 {
        let only = groups.keys().next().cloned().flatten();
        let r = match only {
            Some(to) if to != me => forward(rt, &to, rebuild_request(&parts, bytes.clone())).await,
            _ => run_local_bulk(store, parts, bytes, next).await,
        };
        return wait_for_made(store, &made, r).await;
    }
    // otherwise each node answers for its own part, all at once
    let mut here: Option<(Vec<usize>, String)> = None;
    let mut waits = Vec::new();
    for (node, (order, part)) in groups {
        match node {
            Some(to) if to != me => {
                let r = rebuild_request(&parts, axum::body::Bytes::from(part));
                let rt = rt.clone();
                waits.push(tokio::spawn(async move {
                    let answer = forward(&rt, &to, r).await;
                    (order, answer)
                }));
            }
            _ => match here.as_mut() {
                Some((o, b)) => {
                    o.extend(order);
                    b.push_str(&part);
                }
                None => here = Some((order, part)),
            },
        }
    }
    let mut answers: Vec<(Vec<usize>, Response)> = Vec::new();
    if let Some((order, part)) = here {
        let r = run_local_bulk(store, parts.clone(), axum::body::Bytes::from(part), next).await;
        answers.push((order, r));
    }
    for w in waits {
        if let Ok(a) = w.await {
            answers.push(a);
        }
    }
    let mut items: Vec<Option<Value>> = vec![None; count];
    let mut errors = false;
    let mut took = 0u64;
    let mut ingest_took: Option<u64> = None;
    for (order, r) in answers {
        let status = r.status();
        let body = axum::body::to_bytes(r.into_body(), crate::api::max_content_bytes() as usize)
            .await
            .unwrap_or_default();
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        // a part that failed outright is the whole bulk's answer: the caller
        // is told what went wrong rather than given a half-written body
        if !status.is_success() {
            let mut out = Response::new(Body::from(body));
            *out.status_mut() = status;
            out.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            return out;
        }
        errors |= v.get("errors").and_then(|e| e.as_bool()).unwrap_or(false);
        took = took.max(v.get("took").and_then(|t| t.as_u64()).unwrap_or(0));
        if let Some(t) = v.get("ingest_took").and_then(|t| t.as_u64()) {
            ingest_took = Some(ingest_took.unwrap_or(0).max(t));
        }
        if let Some(list) = v.get("items").and_then(|i| i.as_array()) {
            for (slot, item) in order.iter().zip(list.iter()) {
                if let Some(cell) = items.get_mut(*slot) {
                    *cell = Some(item.clone());
                }
            }
        }
    }
    let mut out = json!({
        "took": took,
        "errors": errors,
        "items": items.into_iter().map(|i| i.unwrap_or(Value::Null)).collect::<Vec<_>>(),
    });
    if let Some(t) = ingest_took {
        out["ingest_took"] = json!(t);
    }
    let mut r = Response::new(Body::from(out.to_string()));
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    wait_for_made(store, &made, r).await
}

/// Hold the answer until this node knows the indices the bulk made, so the
/// request after it does not find them missing.
async fn wait_for_made(store: &Store, made: &[String], response: Response) -> Response {
    if made.is_empty() || !response.status().is_success() {
        return response;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let known = super::with_state(|s| {
            made.iter().all(|n| s.indices.contains_key(n) || !resolve(s, store, n).is_empty())
        });
        if known || std::time::Instant::now() >= deadline {
            return response;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// The request again, with a body of its own.
fn rebuild_request(parts: &axum::http::request::Parts, body: axum::body::Bytes) -> Request {
    let mut b = axum::http::Request::builder().method(parts.method.clone()).uri(parts.uri.clone());
    for (k, v) in parts.headers.iter() {
        b = b.header(k, v);
    }
    let mut r = b.body(Body::from(body)).unwrap_or_default();
    *r.extensions_mut() = parts.extensions.clone();
    r
}

async fn run_local_bulk(
    store: &Store,
    parts: axum::http::request::Parts,
    bytes: axum::body::Bytes,
    next: Next,
) -> Response {
    run_with_replication(store, rebuild_request(&parts, bytes), next).await
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
        // a listing whose numbers only a holder can give goes to every node
        assert_eq!(classify(&get, "/_cat/shards"), Target::Broadcast(None));
        assert_eq!(classify(&get, "/_cat/indices"), Target::Broadcast(None));
        assert_eq!(classify(&get, "/_cat/health"), Target::Local);
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

/// Add one answer's counters into another: numbers add, objects merge, and
/// anything else keeps what the first node said.
fn add_into(acc: &mut Value, other: &Value) {
    match (acc, other) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                match a.get_mut(k) {
                    Some(slot) => add_into(slot, v),
                    None => {
                        a.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (Value::Number(a), Value::Number(b)) => {
            let sum = a.as_f64().unwrap_or(0.0) + b.as_f64().unwrap_or(0.0);
            if a.is_i64() && b.is_i64() {
                *a = serde_json::Number::from(sum as i64);
            } else if let Some(n) = serde_json::Number::from_f64(sum) {
                *a = n;
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            for (i, v) in b.iter().enumerate() {
                match a.get_mut(i) {
                    Some(slot) => add_into(slot, v),
                    None => a.push(v.clone()),
                }
            }
        }
        _ => {}
    }
}

/// Read one document of an index this node holds no copy of.
///
/// A terms lookup names a document in another index, and on a cluster that
/// index may be anywhere: the node holding it is asked for the document the
/// way any other request would be forwarded.
pub fn fetch_document(index: &str, id: &str) -> Option<Value> {
    let rt = super::runtime()?;
    let me = rt.local();
    let holder = super::with_state(|s| {
        s.nodes.keys().find(|n| **n != me && held_here(s, n, index)).cloned()
    })?;
    let ask = json!({
        "method": "GET",
        "uri": format!("/{index}/_doc/{id}"),
        "headers": [],
        "body": "",
        "caller": crate::security::layer::current_caller().unwrap_or_default(),
    });
    // the call runs on the runtime and this thread waits for it: the search
    // that asks for the document may already be inside a blocking section,
    // and a nested block would deadlock rather than answer
    let (tx, rx) = std::sync::mpsc::channel();
    let body = serde_json::to_vec(&ask).unwrap_or_default();
    let rt2 = rt.clone();
    let to = holder.clone();
    super::handle()?.spawn(async move {
        let answer = rt2.call(&to, FORWARD, body, std::time::Duration::from_secs(10)).await;
        let _ = tx.send(answer);
    });
    let answer = rx.recv_timeout(std::time::Duration::from_secs(12)).ok().flatten()?;
    if answer.kind == Kind::Error {
        return None;
    }
    let v: Value = serde_json::from_slice(&answer.body).ok()?;
    let body = v.get("body").and_then(|b| b.as_str())?;
    let doc: Value = serde_json::from_str(body).ok()?;
    doc.get("_source").cloned()
}
