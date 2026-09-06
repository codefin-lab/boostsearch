//! A write reaches every copy: the primary applies it and hands it to the
//! replica copies with the version, sequence number and term it gave it,
//! and answers once the copies that count have taken it. The policy of
//! what counts and where reads may go are parameters (ADR 0003): one value
//! each ships, the values OpenSearch has.
//!
//! The handlers on this node write through the store as they always did;
//! each write they make is recorded in a buffer scoped to the request, and
//! the forwarding layer copies the buffer out before the answer leaves,
//! patching `_shards` in the answer with how many copies took it. A copy
//! that fails is reported to the cluster manager, which fails it. A copy
//! the manager has just placed on a node is filled from the primary by a
//! scan of its documents, in order of sequence number, before the node
//! reports it started; writes made meanwhile reach it as they happen.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::runtime::DataFuture;
use super::state::{ClusterState, ShardState};
use super::transport::{Envelope, Kind, NodeId};
use crate::store::Store;

/// When a write is acknowledged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AckPolicy {
    /// the primary and every in-sync replica have applied it: OpenSearch's
    AllInSync,
}

/// Where a read may be answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadRouting {
    /// any active copy, which may be behind: OpenSearch's
    AnyActiveCopy,
}

/// The consistency mode this build ships (`index.consistency: linearizable`
/// is version two: a quorum acknowledgement and reads through a lease).
#[derive(Clone, Copy, Debug)]
pub struct Mode {
    pub ack: AckPolicy,
    pub read: ReadRouting,
}

pub const MODE: Mode = Mode { ack: AckPolicy::AllInSync, read: ReadRouting::AnyActiveCopy };

pub const REPLICA_WRITE: &str = "indices:data/write/bulk[r]";
pub const RECOVERY_SCAN: &str = "internal:index/recovery/scan";
/// the files of the primary's last commit, and one chunk of one of them
pub const RECOVERY_FILES: &str = "internal:index/recovery/files";
pub const RECOVERY_FILE: &str = "internal:index/recovery/file";
const CHUNK: u64 = 4 * 1024 * 1024;

/// What the primary knows of its copies: the last sequence number each
/// node acknowledged (its local checkpoint), from which the global
/// checkpoint -- the sequence number every in-sync copy has -- follows.
#[derive(Default)]
pub struct Tracker {
    /// index -> node -> local checkpoint
    checkpoints: BTreeMap<String, BTreeMap<NodeId, u64>>,
}

static TRACKER: std::sync::OnceLock<parking_lot::Mutex<Tracker>> = std::sync::OnceLock::new();

fn tracker() -> &'static parking_lot::Mutex<Tracker> {
    TRACKER.get_or_init(|| parking_lot::Mutex::new(Tracker::default()))
}

impl Tracker {
    pub fn acked(&mut self, index: &str, node: &NodeId, seq: u64) {
        let e = self.checkpoints.entry(index.into()).or_default().entry(node.clone()).or_insert(0);
        *e = (*e).max(seq);
    }

    /// The sequence number every named copy has reached, or the primary's
    /// own when it has no copies to wait for.
    pub fn global_checkpoint(&self, index: &str, primary_max: u64, in_sync: &[NodeId]) -> u64 {
        let mut g = primary_max;
        match self.checkpoints.get(index) {
            Some(m) => {
                for n in in_sync {
                    g = g.min(m.get(n).copied().unwrap_or(0));
                }
            }
            None if !in_sync.is_empty() => g = 0,
            None => {}
        }
        g
    }

    pub fn local_checkpoint(&self, index: &str, node: &NodeId) -> Option<u64> {
        self.checkpoints.get(index).and_then(|m| m.get(node).copied())
    }
}

/// The checkpoints as `_stats` reports them for a primary here.
pub fn checkpoints(index: &str, primary_max: u64) -> (u64, u64) {
    let in_sync: Vec<NodeId> = super::with_state(|s| {
        let me = super::runtime().map(|r| r.local());
        s.routing
            .shards_of(index)
            .filter(|c| {
                !c.primary && matches!(c.state, ShardState::Started | ShardState::Relocating)
            })
            .filter_map(|c| c.node.clone())
            .filter(|n| Some(n) != me.as_ref())
            .collect()
    });
    let t = tracker().lock();
    (primary_max, t.global_checkpoint(index, primary_max, &in_sync))
}

/// One write as the primary made it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplicaOp {
    pub index: String,
    pub id: String,
    pub routing: Option<String>,
    pub version: u64,
    pub seq: u64,
    pub term: u64,
    pub shard: u32,
    /// the document, or nothing for a delete
    pub source: Option<String>,
}

tokio::task_local! {
    /// The writes the request in hand has made, waiting to be copied.
    pub static WRITES: std::cell::RefCell<Vec<ReplicaOp>>;
}

/// Note a write the primary made, if a request is being handled.
pub fn record(op: ReplicaOp) {
    let _ = WRITES.try_with(|w| w.borrow_mut().push(op));
}

/// Where a write to a shard is copied: the replica copies on other nodes,
/// active or still initializing (an initializing copy takes writes so it
/// is caught up when it starts), with whether each one is in sync.
pub fn targets(state: &ClusterState, me: &NodeId, index: &str, _shard: u32) -> Vec<(NodeId, bool)> {
    // a copy is a copy of the whole index: every node holding one, for any
    // shard, takes every write, and is in sync when all it holds is active
    let mut by_node: BTreeMap<NodeId, bool> = BTreeMap::new();
    for c in state.routing.shards_of(index) {
        if !matches!(
            c.state,
            ShardState::Started | ShardState::Relocating | ShardState::Initializing
        ) {
            continue;
        }
        // a primary that is not being moved is the source, not a copy; the
        // source of a move stays the primary until its target takes over
        if c.primary && (c.relocating_node.is_none() || c.state == ShardState::Relocating) {
            continue;
        }
        let Some(n) = c.node.clone() else { continue };
        if n == *me {
            continue;
        }
        let e = by_node.entry(n).or_insert(true);
        *e &= c.state != ShardState::Initializing;
    }
    by_node.into_iter().collect()
}

/// How many copies of an index took a request's writes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ack {
    pub total: usize,
    pub successful: usize,
    pub failed: usize,
    pub failures: Vec<Value>,
    /// a copy's failure or staleness could not be recorded with the manager
    pub manager_unreachable: bool,
}

/// The primary's side: copy the writes out, wait for the answers, and
/// count them per index.
pub async fn replicate(ops: Vec<ReplicaOp>, refresh: &str) -> BTreeMap<String, Ack> {
    let mut acks: BTreeMap<String, Ack> = BTreeMap::new();
    let Some(rt) = super::runtime() else { return acks };
    let me = rt.local();
    let state = super::current_state();
    // (node, index) -> ops; whether the node's copies are in sync
    let mut batches: BTreeMap<(NodeId, String), (Vec<ReplicaOp>, bool)> = BTreeMap::new();
    for op in &ops {
        let m = state.indices.get(&op.index);
        let ack = acks.entry(op.index.clone()).or_default();
        ack.total = 1 + m.map(|m| m.number_of_replicas as usize).unwrap_or(0);
        ack.successful = 1;
        for (node, in_sync) in targets(&state, &me, &op.index, op.shard) {
            let e = batches.entry((node, op.index.clone())).or_insert_with(|| (Vec::new(), true));
            e.0.push(op.clone());
            e.1 &= in_sync;
        }
    }
    // even with no copy to write to there is bookkeeping to do: an in-sync
    // copy whose node is down did not take this write either, and its
    // allocation id has to leave the in-sync set before the node comes back
    // and is handed the primary as though it had everything
    // every copy is written to at once; the answers are gathered in turn
    let mut waits = Vec::new();
    for ((node, index), (batch, in_sync)) in batches {
        let rt = rt.clone();
        let body = serde_json::to_vec(&json!({"index": index, "refresh": refresh, "ops": batch}))
            .unwrap_or_default();
        waits.push(tokio::spawn(async move {
            let answer = call_while_member(&rt, &node, REPLICA_WRITE, body).await;
            (node, index, in_sync, answer)
        }));
    }
    let mut answers = Vec::new();
    for w in waits {
        if let Ok(a) = w.await {
            answers.push(a);
        }
    }
    let manager = state.cluster_manager.clone();
    // a copy that failed, or fell out of sync, is the manager's to record
    // before this write may be acknowledged: a primary that cannot reach the
    // manager acknowledges nothing (its own copy may be the stale one)
    let mut manager_unreachable = false;
    // which nodes answered for each index; the counts follow the shards
    // written, since `_shards` speaks of a shard's copies
    let mut acked_nodes: BTreeMap<String, Vec<NodeId>> = BTreeMap::new();
    for (node, index, in_sync, answer) in answers {
        let ack = acks.entry(index.clone()).or_default();
        let failure: Option<String> = match &answer {
            Ok(e) if e.kind == Kind::Response => None,
            Ok(e) => Some(String::from_utf8_lossy(&e.body).into_owned()),
            Err(Left) => Some("the node left the cluster".into()),
            Err(NoAnswer) => Some("no answer from the node".into()),
        };
        // a copy whose node the manager has already removed is not a failed
        // copy: it is unassigned, and the write goes on without it
        let left = matches!(answer, Err(Left));
        match failure {
            None => {
                // every node that took the write, in sync or still filling:
                // a copy that took it is not a copy that missed it
                acked_nodes.entry(index.clone()).or_default().push(node.clone());
                let _ = in_sync;
                let max_seq =
                    ops.iter().filter(|o| o.index == index).map(|o| o.seq).max().unwrap_or(0);
                tracker().lock().acked(&index, &node, max_seq);
            }
            Some(reason) => {
                if in_sync && !left {
                    ack.failed += 1;
                    ack.failures.push(json!({
                        "_index": index,
                        "_node": node.as_str(),
                        "reason": {"type": "exception", "reason": reason},
                        "status": "INTERNAL_SERVER_ERROR",
                        "primary": false,
                    }));
                }
                // the copy is no good: the manager hears of it and fails it
                if let Some(mgr) = &manager {
                    let copies: Vec<(u32, String)> = state
                        .routing
                        .shards_of(&index)
                        .filter(|c| c.node.as_ref() == Some(&node) && !c.primary)
                        .filter_map(|c| c.allocation_id.clone().map(|a| (c.shard, a)))
                        .collect();
                    for (shard, aid) in copies {
                        let body = json!({"index": index, "shard": shard, "allocation_id": aid,
                            "message": format!("replication to [{}] failed: {reason}", node.as_str())});
                        let answer = rt
                            .call(
                                mgr,
                                super::coordinator::SHARD_FAILED,
                                serde_json::to_vec(&body).unwrap_or_default(),
                                std::time::Duration::from_secs(10),
                            )
                            .await;
                        if !matches!(answer, Some(ref a) if a.kind == Kind::Response) {
                            if std::env::var("BOOSTSEARCH_CLUSTER_DEBUG").is_ok() {
                                eprintln!(
                                    "boostsearch: the manager would not record a copy of [{index}]: {}",
                                    match &answer {
                                        Some(a) => String::from_utf8_lossy(&a.body).into_owned(),
                                        None => "no answer".to_string(),
                                    }
                                );
                            }
                            manager_unreachable = true;
                        }
                    }
                }
            }
        }
    }
    // a copy that was in sync and did not take this write is in sync no
    // more: the manager is told, so a copy that missed writes can never
    // be handed the primary on its own
    if let Some(mgr) = &manager {
        for index in acks.keys() {
            let Some(m) = state.indices.get(index) else { continue };
            let acked = acked_nodes.get(index).cloned().unwrap_or_default();
            let mut fine: Vec<String> = Vec::new();
            for c in state.routing.shards_of(index) {
                let took = c.node.as_ref() == Some(&me)
                    || c.node.as_ref().map(|n| acked.contains(n)).unwrap_or(false);
                if took
                    && matches!(
                        c.state,
                        ShardState::Started | ShardState::Relocating | ShardState::Initializing
                    )
                    && let Some(a) = &c.allocation_id
                {
                    fine.push(a.clone());
                }
            }
            // ids that belong to a copy the routing still has: an id left in
            // the set for a copy that is gone is the cluster's memory of
            // where the data was, and there is nothing to take out of the set
            let placed: Vec<String> = state
                .routing
                .shards_of(index)
                .filter(|c| c.state != ShardState::Unassigned)
                .filter_map(|c| c.allocation_id.clone())
                .collect();
            for (shard, ids) in &m.in_sync_allocations {
                for id in ids {
                    if !fine.contains(id) && placed.contains(id) {
                        let body = json!({"index": index, "shard": shard, "allocation_id": id});
                        let answer = rt
                            .call(
                                mgr,
                                super::coordinator::SHARD_STALE,
                                serde_json::to_vec(&body).unwrap_or_default(),
                                std::time::Duration::from_secs(10),
                            )
                            .await;
                        if !matches!(answer, Some(ref a) if a.kind == Kind::Response) {
                            if std::env::var("BOOSTSEARCH_CLUSTER_DEBUG").is_ok() {
                                eprintln!(
                                    "boostsearch: the manager would not record a copy of [{index}]: {}",
                                    match &answer {
                                        Some(a) => String::from_utf8_lossy(&a.body).into_owned(),
                                        None => "no answer".to_string(),
                                    }
                                );
                            }
                            manager_unreachable = true;
                        }
                    }
                }
            }
        }
    }
    // successful copies of a shard: the primary and the in-sync replica
    // copies of that shard whose node answered; over a request that wrote
    // to several shards, the fewest
    for (index, ack) in acks.iter_mut() {
        let acked = acked_nodes.get(index).cloned().unwrap_or_default();
        let shards: std::collections::BTreeSet<u32> =
            ops.iter().filter(|o| o.index == *index).map(|o| o.shard).collect();
        let mut fewest: Option<usize> = None;
        for shard in shards {
            let holders = state
                .routing
                .shards_of(index)
                .filter(|c| {
                    c.shard == shard
                        && !c.primary
                        && matches!(c.state, ShardState::Started | ShardState::Relocating)
                })
                .filter(|c| c.node.as_ref().map(|n| acked.contains(n)).unwrap_or(false))
                .count();
            fewest = Some(fewest.map_or(holders, |f| f.min(holders)));
        }
        ack.successful = 1 + fewest.unwrap_or(0);
        ack.successful = ack.successful.min(ack.total.max(1));
        if manager_unreachable {
            ack.manager_unreachable = true;
        }
    }
    acks
}

/// `_shards` in a write's answer, by what the copies said: the answer to a
/// single document, or every item of a bulk.
pub fn patch_shards(v: &mut Value, acks: &BTreeMap<String, Ack>) {
    fn patch_one(item: &mut Value, acks: &BTreeMap<String, Ack>) {
        let Some(index) = item.get("_index").and_then(|i| i.as_str()).map(|s| s.to_string()) else {
            return;
        };
        let Some(ack) = acks.get(&index) else { return };
        if let Some(shards) = item.get_mut("_shards") {
            shards["total"] = json!(ack.total);
            shards["successful"] = json!(ack.successful);
            shards["failed"] = json!(ack.failed);
            if !ack.failures.is_empty() {
                shards["failures"] = json!(ack.failures);
            }
        }
    }
    if v.get("_shards").is_some() {
        patch_one(v, acks);
    }
    if let Some(items) = v.get_mut("items").and_then(|i| i.as_array_mut()) {
        for item in items {
            if let Some(o) = item.as_object_mut() {
                for (_, inner) in o.iter_mut() {
                    patch_one(inner, acks);
                }
            }
        }
    }
}

/// This node's allocation id for an index, when it holds a copy.
fn here_id(state: &ClusterState, me: &NodeId, index: &str) -> Option<String> {
    state
        .routing
        .shards_of(index)
        .find(|c| c.node.as_ref() == Some(me))
        .and_then(|c| c.allocation_id.clone())
}

/// After a handler wrote: copy out, then say so in the answer.
pub async fn finish(
    response: axum::response::Response,
    ops: Vec<ReplicaOp>,
    refresh: &str,
) -> axum::response::Response {
    // a node that has lost the cluster manager knows nothing of what the
    // cluster decided while it was away: the primary it thinks it holds may
    // be somebody else's now, and a write it acknowledged alone would be
    // thrown away. OpenSearch blocks writes the same way, on the
    // `no cluster-manager` block its checks raise.
    if !super::has_manager() {
        return crate::api::err(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "cluster_block_exception",
            "blocked by: [SERVICE_UNAVAILABLE/2/no cluster-manager];",
        );
    }
    // nothing to copy to and nothing in the in-sync set to retire: the
    // answer stands as the handler wrote it
    let nothing_to_do = super::with_state(|s| {
        let me = super::runtime().map(|r| r.local());
        let Some(me) = me else { return true };
        ops.iter().all(|op| {
            targets(s, &me, &op.index, op.shard).is_empty()
                && s.indices
                    .get(&op.index)
                    .map(|m| {
                        m.in_sync_allocations
                            .values()
                            .flatten()
                            .all(|id| Some(id) == here_id(s, &me, &op.index).as_ref())
                    })
                    .unwrap_or(true)
        })
    });
    if nothing_to_do {
        return response;
    }
    let acks = replicate(ops, refresh).await;
    // a copy that refused this node's term: this node is no primary any
    // more, and the write did not happen as far as the cluster is concerned
    let stale = acks.values().any(|a| {
        a.failures.iter().any(|f| {
            f.pointer("/reason/reason")
                .and_then(|r| r.as_str())
                .map(|r| r.contains("stale primary term"))
                .unwrap_or(false)
        })
    });
    if stale {
        return crate::api::err(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "unavailable_shards_exception",
            "the primary that took this write is no longer the primary (its term is stale); retry",
        );
    }
    if acks.values().any(|a| a.manager_unreachable) {
        return crate::api::err(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "unavailable_shards_exception",
            "a copy did not take this write and the cluster manager could not be told; not acknowledged, retry",
        );
    }
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
    let mut v: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return axum::response::Response::from_parts(parts, axum::body::Body::from(bytes));
        }
    };
    patch_shards(&mut v, &acks);
    let out = serde_json::to_vec(&v).unwrap_or_default();
    let mut parts = parts;
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    axum::response::Response::from_parts(parts, axum::body::Body::from(out))
}

/// The replica's side, and the recovery scan a new copy asks the primary for.
pub fn install(store: Store) {
    let Some(rt) = super::runtime() else { return };
    let me = rt.local();
    let s = store.clone();
    let from = me.clone();
    rt.register(
        REPLICA_WRITE,
        Arc::new(move |e: Envelope| -> DataFuture {
            let store = s.clone();
            let from = from.clone();
            Box::pin(async move {
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let index = v.get("index").and_then(|i| i.as_str()).unwrap_or("").to_string();
                let refresh = v.get("refresh").and_then(|r| r.as_str()).unwrap_or("").to_string();
                let ops: Vec<ReplicaOp> = v
                    .get("ops")
                    .and_then(|o| serde_json::from_value(o.clone()).ok())
                    .unwrap_or_default();
                // a copy being swapped in by recovery is away for a moment
                let mut waited = 0;
                while store.get(&index).is_none() && waited < 40 {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    waited += 1;
                }
                // a primary of an older term is no primary: its writes are refused
                let known_term = super::primary_term(&index, 0);
                if ops.iter().any(|op| op.term < known_term) {
                    return e.error(
                        from,
                        &format!(
                            "stale primary term for [{index}]: {} < {known_term}",
                            ops.iter().map(|o| o.term).min().unwrap_or(0)
                        ),
                    );
                }
                // a copy being filled takes the write when the seed is done
                if park(&index, &ops) {
                    let body =
                        serde_json::to_vec(&json!({"applied": ops.len()})).unwrap_or_default();
                    return e.response(from, body);
                }
                let result = tokio::task::spawn_blocking(move || {
                    let Some(st) = store.get(&index) else {
                        return Err(format!("no copy of [{index}] on this node"));
                    };
                    let mut g = st.write();
                    let mut applied = 0usize;
                    for op in &ops {
                        if crate::api::doc::apply_replicated(&mut g, op) {
                            applied += 1;
                        }
                    }
                    g.sync_translog();
                    if refresh == "true" || refresh == "wait_for" || refresh.is_empty() && false {
                        let _ = g.refresh();
                    }
                    Ok(applied)
                })
                .await
                .unwrap_or_else(|e| Err(format!("replica write panicked: {e}")));
                match result {
                    Ok(n) => e.response(
                        from,
                        serde_json::to_vec(&json!({"applied": n})).unwrap_or_default(),
                    ),
                    Err(msg) => e.error(from, &msg),
                }
            })
        }),
    );
    install_files(&rt, &store, &me);
    let s = store.clone();
    let from = me.clone();
    rt.register(
        RECOVERY_SCAN,
        Arc::new(move |e: Envelope| -> DataFuture {
            let store = s.clone();
            let from = from.clone();
            Box::pin(async move {
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let index = v.get("index").and_then(|i| i.as_str()).unwrap_or("").to_string();
                let shard = v.get("shard").and_then(|s| s.as_u64()).unwrap_or(0) as u32;
                let from_seq = v.get("from_seq").and_then(|s| s.as_u64()).unwrap_or(0);
                let size = v.get("size").and_then(|s| s.as_u64()).unwrap_or(1000) as usize;
                let result = tokio::task::spawn_blocking(move || {
                    let Some(st) = store.get(&index) else {
                        return Err(format!("no index [{index}] on this node"));
                    };
                    let g = st.read();
                    Ok(crate::api::doc::scan_replicated(&g, shard, from_seq, size))
                })
                .await
                .unwrap_or_else(|e| Err(format!("scan panicked: {e}")));
                match result {
                    Ok((ops, next)) => e.response(
                        from,
                        serde_json::to_vec(&json!({"ops": ops, "next_seq": next}))
                            .unwrap_or_default(),
                    ),
                    Err(msg) => e.error(from, &msg),
                }
            })
        }),
    );
}

/// The primary's side of a file-based recovery: commit, then list and serve files.
fn install_files(rt: &super::runtime::Runtime, store: &Store, me: &NodeId) {
    let s = store.clone();
    let from = me.clone();
    rt.register(
        RECOVERY_FILES,
        Arc::new(move |e: Envelope| -> DataFuture {
            let store = s.clone();
            let from = from.clone();
            Box::pin(async move {
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let index = v.get("index").and_then(|i| i.as_str()).unwrap_or("").to_string();
                let result = tokio::task::spawn_blocking(move || -> Result<Value, String> {
                    let Some(st) = store.get(&index) else {
                        return Err(format!("no index [{index}] on this node"));
                    };
                    let (dir, max_seq) = {
                        let mut g = st.write();
                        // everything acknowledged goes into one commit the copy can take whole
                        g.refresh().map_err(|e| e.to_string())?;
                        (g.path.clone(), g.seq_no)
                    };
                    let Some(dir) = dir else { return Err("the index is not on disk".into()) };
                    let mut files = Vec::new();
                    for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
                        let entry = entry.map_err(|e| e.to_string())?;
                        let name = entry.file_name().to_string_lossy().to_string();
                        let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
                        if name == crate::store::TRANSLOG || name.ends_with(".lock") || !is_file {
                            continue;
                        }
                        let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
                        files.push(json!({"name": name, "len": len}));
                    }
                    Ok(json!({"files": files, "max_seq": max_seq}))
                })
                .await
                .unwrap_or_else(|e| Err(format!("listing panicked: {e}")));
                match result {
                    Ok(v) => e.response(from, serde_json::to_vec(&v).unwrap_or_default()),
                    Err(msg) => e.error(from, &msg),
                }
            })
        }),
    );
    let s = store.clone();
    let from = me.clone();
    rt.register(
        RECOVERY_FILE,
        Arc::new(move |e: Envelope| -> DataFuture {
            let store = s.clone();
            let from = from.clone();
            Box::pin(async move {
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let index = v.get("index").and_then(|i| i.as_str()).unwrap_or("").to_string();
                let name = v.get("name").and_then(|i| i.as_str()).unwrap_or("").to_string();
                let offset = v.get("offset").and_then(|i| i.as_u64()).unwrap_or(0);
                let len = v.get("len").and_then(|i| i.as_u64()).unwrap_or(CHUNK).min(CHUNK);
                let result = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
                    use std::io::{Read, Seek};
                    if name.contains('/') || name.contains("..") {
                        return Err("bad file name".into());
                    }
                    let Some(st) = store.get(&index) else {
                        return Err(format!("no index [{index}] on this node"));
                    };
                    let dir = st.read().path.clone().ok_or("the index is not on disk")?;
                    let mut f = std::fs::File::open(dir.join(&name)).map_err(|e| e.to_string())?;
                    f.seek(std::io::SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
                    let mut buf = vec![0u8; len as usize];
                    let mut got = 0;
                    while got < buf.len() {
                        let n = f.read(&mut buf[got..]).map_err(|e| e.to_string())?;
                        if n == 0 {
                            break;
                        }
                        got += n;
                    }
                    buf.truncate(got);
                    Ok(buf)
                })
                .await
                .unwrap_or_else(|e| Err(format!("read panicked: {e}")));
                match result {
                    Ok(bytes) => e.response(from, bytes),
                    Err(msg) => e.error(from, &msg),
                }
            })
        }),
    );
}

/// Fill a copy from the primary's committed files, then replay what the
/// copy here was holding meanwhile. `Ok(false)` when the primary is not on
/// disk, so the document scan must do it.
async fn seed_from_files(store: &Store, index: &str, primary: &NodeId) -> Result<bool, String> {
    let Some(rt) = super::runtime() else { return Ok(false) };
    let body = serde_json::to_vec(&json!({"index": index})).unwrap_or_default();
    let Some(answer) =
        rt.call(primary, RECOVERY_FILES, body, std::time::Duration::from_secs(120)).await
    else {
        return Err(format!("recovery of [{index}]: no answer to the file listing"));
    };
    if answer.kind != Kind::Response {
        let why = String::from_utf8_lossy(&answer.body).into_owned();
        if why.contains("not on disk") {
            return Ok(false);
        }
        return Err(format!("recovery of [{index}]: {why}"));
    }
    let v: Value = serde_json::from_slice(&answer.body).unwrap_or(Value::Null);
    let files: Vec<(String, u64)> = v
        .get("files")
        .and_then(|f| f.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|f| {
                    Some((f.get("name")?.as_str()?.to_string(), f.get("len")?.as_u64()?))
                })
                .collect()
        })
        .unwrap_or_default();
    let Some(dest) = store.index_dir(index) else { return Ok(false) };
    let tmp = dest.with_extension("recovering");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    for (name, len) in &files {
        let mut offset = 0u64;
        let mut out = std::fs::File::create(tmp.join(name)).map_err(|e| e.to_string())?;
        use std::io::Write;
        while offset < *len {
            let ask = serde_json::to_vec(
                &json!({"index": index, "name": name, "offset": offset, "len": CHUNK}),
            )
            .unwrap_or_default();
            let Some(chunk) =
                rt.call(primary, RECOVERY_FILE, ask, std::time::Duration::from_secs(120)).await
            else {
                return Err(format!("recovery of [{index}]: no answer for [{name}] at {offset}"));
            };
            if chunk.kind != Kind::Response {
                return Err(format!(
                    "recovery of [{index}]: [{name}]: {}",
                    String::from_utf8_lossy(&chunk.body)
                ));
            }
            if chunk.body.is_empty() {
                break;
            }
            out.write_all(&chunk.body).map_err(|e| e.to_string())?;
            offset += chunk.body.len() as u64;
        }
        if offset == 0 && *len > 0 {
            return Err(format!("recovery of [{index}]: [{name}] came back empty"));
        }
    }
    // the copy that was here goes, the files take its place, and what its
    // translog held (writes copied in while the files travelled) is replayed
    let store2 = store.clone();
    let name = index.to_string();
    let tmp2 = tmp.clone();
    let replayed = tokio::task::spawn_blocking(move || -> Result<usize, String> {
        let held = store2.adopt(&name, &tmp2).map_err(|e| e.to_string())?;
        let Some(st) = store2.get(&name) else {
            return Err(format!("[{name}] did not open after recovery"));
        };
        let mut g = st.write();
        let mut n = 0;
        for rec in held {
            let Some(id) = rec.get("id").and_then(|v| v.as_str()) else { continue };
            let op = ReplicaOp {
                index: name.clone(),
                id: id.to_string(),
                routing: rec.get("routing").and_then(|v| v.as_str()).map(|s| s.to_string()),
                version: rec.get("version").and_then(|v| v.as_u64()).unwrap_or(1),
                seq: rec.get("seq").and_then(|v| v.as_u64()).unwrap_or(0),
                term: 1,
                shard: 0,
                source: match rec.get("source") {
                    Some(Value::Null) | None => None,
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(v) => Some(v.to_string()),
                },
            };
            if crate::api::doc::apply_replicated(&mut g, &op) {
                n += 1;
            }
        }
        g.sync_translog();
        let _ = g.refresh();
        Ok(n)
    })
    .await
    .unwrap_or_else(|e| Err(format!("adopting panicked: {e}")));
    replayed?;
    Ok(true)
}

/// One recovery per index at a time on a node: two copies of one index
/// placed here together share the files, so the second waits for the
/// first and finds them.
/// The recovery running for each shard, if one is: the lock somebody takes to
/// be the one doing it, and the node it is seeding from once that is settled.
type Recoveries = BTreeMap<String, Arc<tokio::sync::Mutex<Option<String>>>>;

static RECOVERING: std::sync::OnceLock<parking_lot::Mutex<Recoveries>> = std::sync::OnceLock::new();

fn recovery_lock(index: &str) -> Arc<tokio::sync::Mutex<Option<String>>> {
    let m = RECOVERING.get_or_init(|| parking_lot::Mutex::new(BTreeMap::new()));
    m.lock()
        .entry(index.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
        .clone()
}

/// Writes that reached a copy while it was being filled.
///
/// The seed throws away what was on this node and fills the copy from the
/// primary, so a write applied in the middle of that goes out with the old
/// copy and is never asked for again -- the scan has already passed its
/// sequence number. It waits here instead, and the seed applies what waited
/// as the last thing it does, under the lock that closes the recovery.
static ARRIVED: std::sync::OnceLock<parking_lot::Mutex<BTreeMap<String, Vec<ReplicaOp>>>> =
    std::sync::OnceLock::new();

fn arrived() -> &'static parking_lot::Mutex<BTreeMap<String, Vec<ReplicaOp>>> {
    ARRIVED.get_or_init(|| parking_lot::Mutex::new(BTreeMap::new()))
}

/// A write for a copy that is being filled: it waits for the seed. False
/// when no recovery is running, and the caller applies it itself.
fn park(index: &str, ops: &[ReplicaOp]) -> bool {
    let mut m = arrived().lock();
    match m.get_mut(index) {
        Some(waiting) => {
            waiting.extend(ops.iter().cloned());
            true
        }
        None => false,
    }
}

/// Fill a copy the manager placed here: from the primary's files when it
/// has them on disk, else from a scan of its documents.
pub async fn seed_replica(
    store: &Store,
    index: &str,
    shard: u32,
    allocation_id: &str,
    primary: &NodeId,
) -> Result<(), String> {
    let lock = recovery_lock(index);
    let mut done = lock.lock().await;
    // the same copy asked for twice while a publication is repeated is one
    // recovery; a copy with another allocation id is another copy, and is
    // filled however lately the last one was -- what is on this node may be
    // a copy the cluster left behind, missing everything written since
    if done.as_deref() == Some(allocation_id) && store.get(index).is_some() {
        return Ok(());
    }
    arrived().lock().insert(index.to_string(), Vec::new());
    let notes = std::env::var("BOOSTSEARCH_CLUSTER_DEBUG").is_ok();
    let before = store.get(index).map(|st| st.read().live_ids.len()).unwrap_or(0);
    let r = seed_replica_inner(store, index, shard, primary).await;
    let r = match r {
        Ok(()) => apply_what_waited(store, index).await,
        Err(why) => {
            arrived().lock().remove(index);
            Err(why)
        }
    };
    if r.is_ok() {
        *done = Some(allocation_id.to_string());
    }
    if notes {
        let after = store.get(index).map(|st| st.read().live_ids.len()).unwrap_or(0);
        eprintln!(
            "boostsearch: {} filled [{index}] as {allocation_id}: {before} documents here before, {after} after ({})",
            super::clock().wall(),
            match &r {
                Ok(()) => "done".to_string(),
                Err(why) => why.clone(),
            }
        );
    }
    r
}

/// The end of a recovery: what waited goes in, and the copy is a copy that
/// takes its writes as they come. Draining and closing happen under one
/// lock, so a write cannot slip between the last drain and the close.
async fn apply_what_waited(store: &Store, index: &str) -> Result<(), String> {
    let store = store.clone();
    let name = index.to_string();
    tokio::task::spawn_blocking(move || {
        loop {
            let batch = {
                let mut m = arrived().lock();
                match m.get_mut(&name) {
                    Some(waiting) if !waiting.is_empty() => std::mem::take(waiting),
                    // nothing waiting: close the recovery while the lock is held
                    _ => {
                        m.remove(&name);
                        return Ok(());
                    }
                }
            };
            let Some(st) = store.get(&name) else {
                arrived().lock().remove(&name);
                return Err(format!("no copy of [{name}] here to finish"));
            };
            let mut g = st.write();
            for op in &batch {
                crate::api::doc::apply_replicated(&mut g, op);
            }
            g.sync_translog();
        }
    })
    .await
    .unwrap_or_else(|e| Err(format!("finishing the recovery of [{index}] panicked: {e}")))
}

async fn seed_replica_inner(
    store: &Store,
    index: &str,
    shard: u32,
    primary: &NodeId,
) -> Result<(), String> {
    let me = super::runtime().map(|r| r.local());
    if let Some(me) = &me {
        if primary == me {
            // the manager placed a copy here and named this node the primary
            // of it: this node's own state is not what the manager published,
            // and filling from itself would leave the copy as it stands
            return Err(format!("[{index}][{shard}]: this node cannot fill a copy from itself"));
        }
        match seed_from_files(store, index, primary).await {
            // the files are the primary's last commit; what it took after
            // that comes over as documents
            Ok(true) => {
                let from = store.get(index).map(|st| st.read().seq_no).unwrap_or(0);
                return catch_up_by_scan(store, index, shard, from, primary, false).await;
            }
            Ok(false) => {}
            Err(why) => {
                // the files did not come: the documents will
                eprintln!("boostsearch: {why}; scanning instead");
            }
        }
    }
    seed_by_scan(store, index, shard, primary).await
}

/// Fill a copy from a scan of the primary's documents, in sequence order,
/// starting from nothing: what was here before may hold writes the
/// primary never took.
pub async fn seed_by_scan(
    store: &Store,
    index: &str,
    shard: u32,
    primary: &NodeId,
) -> Result<(), String> {
    // whether what was here could not be emptied, so the pages must overwrite
    let mut stubborn = false;
    {
        let meta = super::with_state(|s| s.indices.get(index).cloned());
        let store2 = store.clone();
        let name = index.to_string();
        let primary_here = super::runtime().map(|r| r.local()).as_ref() == Some(primary);
        if !primary_here && let Some(meta) = meta {
            let made = tokio::task::spawn_blocking(move || {
                    store2.drop_local(&name);
                    // files a half-finished recovery left behind: nothing holds
                    // them open once the store has let the index go, and the
                    // new copy cannot be opened on top of them
                    if store2.get(&name).is_none()
                        && let Some(dir) = store2.index_dir(&name) {
                            let _ = std::fs::remove_dir_all(&dir);
                        }
                    let mut settings = meta.settings.clone();
                    if let Some(idx) = settings.get_mut("index").and_then(|v| v.as_object_mut()) {
                        for k in ["creation_date", "provided_name", "version"] {
                            idx.remove(k);
                        }
                        idx.insert("uuid".into(), json!(meta.uuid));
                    }
                    let body = json!({"settings": settings, "mappings": meta.mappings, "aliases": meta.aliases});
                    // the empty index the documents will be applied to: if it
                    // cannot be made, the recovery says so rather than failing
                    // page by page with nothing here to apply them to
                    match store2.create(&name, &body) {
                        Ok(()) => Ok(()),
                        Err(e) => {
                            // the old copy had not finished being dropped: it
                            // is dropped again, and what the scan sends will
                            // overwrite whatever is left standing
                            std::thread::sleep(std::time::Duration::from_millis(200));
                            store2.drop_local(&name);
                            match store2.create(&name, &body) {
                                Ok(()) => Ok(()),
                                Err(_) if store2.get(&name).is_some() => Err(String::new()),
                                Err(_) => {
                                    Err(format!("could not make a copy of [{name}] here: {e}"))
                                }
                            }
                        }
                    }
                })
                .await
                .unwrap_or_else(|e| Err(format!("making a copy of [{index}] panicked: {e}")));
            match made {
                Ok(()) => {}
                // an empty message means "it is still standing": the pages
                // that follow overwrite it rather than being skipped as
                // versions already held
                Err(e) if e.is_empty() => stubborn = true,
                Err(e) => return Err(e),
            }
        }
    }
    catch_up_by_scan(store, index, shard, 0, primary, stubborn).await
}

/// Ask the primary for everything from a sequence number on, and apply it
/// here. A copy filled from the primary's files holds what the primary had
/// committed, not the writes it had taken and not yet committed, so a file
/// recovery catches up this way from where its files end.
pub async fn catch_up_by_scan(
    store: &Store,
    index: &str,
    shard: u32,
    from: u64,
    primary: &NodeId,
    overwrite: bool,
) -> Result<(), String> {
    let Some(rt) = super::runtime() else { return Ok(()) };
    let me = rt.local();
    if *primary == me {
        return Ok(());
    }
    let primary = primary.clone();
    let mut from_seq = from;
    let mut tries = 0;
    loop {
        let body = serde_json::to_vec(
            &json!({"index": index, "shard": u32::MAX, "from_seq": from_seq, "size": 2000}),
        )
        .unwrap_or_default();
        let answer =
            rt.call(&primary, RECOVERY_SCAN, body, std::time::Duration::from_secs(60)).await;
        let Some(answer) = answer.filter(|a| a.kind == Kind::Response) else {
            tries += 1;
            if tries > 5 {
                return Err(format!(
                    "recovery of [{index}][{shard}] from {} got no answer",
                    primary.as_str()
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            continue;
        };
        tries = 0;
        let v: Value = serde_json::from_slice(&answer.body).unwrap_or(Value::Null);
        let ops: Vec<ReplicaOp> =
            v.get("ops").and_then(|o| serde_json::from_value(o.clone()).ok()).unwrap_or_default();
        let next = v.get("next_seq").and_then(|n| n.as_u64());
        let store = store.clone();
        let name = index.to_string();
        let applied = tokio::task::spawn_blocking(move || {
            let Some(st) = store.get(&name) else {
                return Err(format!("no copy of [{name}] here"));
            };
            let mut g = st.write();
            for op in &ops {
                if overwrite {
                    crate::api::doc::apply_recovered(&mut g, op);
                } else {
                    crate::api::doc::apply_replicated(&mut g, op);
                }
            }
            g.sync_translog();
            Ok(())
        })
        .await
        .unwrap_or_else(|e| Err(format!("recovery apply panicked: {e}")));
        applied?;
        match next {
            Some(n) if n > from_seq => from_seq = n,
            _ => break,
        }
    }
    // what came in is searchable on the copy once it is refreshed
    let store = store.clone();
    let name = index.to_string();
    let _ = tokio::task::spawn_blocking(move || {
        if let Some(st) = store.get(&name) {
            let _ = st.write().refresh();
        }
    })
    .await;
    Ok(())
}

/// Why a copy write brought nothing back.
enum NoReply {
    /// the manager removed the node while the write was waiting
    Left,
    /// the node stayed a member and did not answer in time
    NoAnswer,
}
use NoReply::{Left, NoAnswer};

/// A call that waits as long as the node is a member of the cluster: a
/// partition or a stopped process is the cluster manager's to notice, and
/// once it has removed the node, the copy there is unassigned rather than
/// failed. OpenSearch's replication waits the same way.
async fn call_while_member(
    rt: &Arc<super::runtime::Runtime>,
    node: &NodeId,
    action: &str,
    body: Vec<u8>,
) -> Result<Envelope, NoReply> {
    let call = rt.call(node, action, body, std::time::Duration::from_secs(60));
    tokio::pin!(call);
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
    loop {
        tokio::select! {
            answer = &mut call => return answer.ok_or(NoAnswer),
            _ = tick.tick() => {
                let member = super::with_state(|s| s.nodes.contains_key(node));
                if !member {
                    return Err(Left);
                }
            }
        }
    }
}

/// What this node holds, sent to the other copies of a shard it has just
/// become the primary of.
///
/// A copy that followed the primary before this one may hold another value
/// for a document -- it took a write this node never did, in a term that is
/// over -- and nothing later would reconcile the two. Every document goes
/// out under the new term, which wins over whatever version stands on the
/// copy. Documents the copy has and this node does not are left alone: they
/// may be writes it took and answered for.
pub async fn resync(
    store: &Store,
    index: &str,
    shard: u32,
    term: u64,
    to: &[NodeId],
) -> Result<(), String> {
    let Some(rt) = super::runtime() else { return Ok(()) };
    let mut from_seq = 0u64;
    let mut sent = 0usize;
    loop {
        let store2 = store.clone();
        let name = index.to_string();
        let page = tokio::task::spawn_blocking(move || {
            let Some(st) = store2.get(&name) else { return (Vec::new(), None) };
            let g = st.read();
            crate::api::doc::scan_replicated(&g, u32::MAX, from_seq, 2000)
        })
        .await
        .unwrap_or((Vec::new(), None));
        let (mut ops, next) = page;
        if ops.is_empty() && next.is_none() {
            break;
        }
        for op in ops.iter_mut() {
            op.term = term;
        }
        sent += ops.len();
        let body = serde_json::to_vec(&json!({"index": index, "refresh": "", "ops": ops}))
            .unwrap_or_default();
        for node in to {
            let _ = rt
                .call(node, REPLICA_WRITE, body.clone(), std::time::Duration::from_secs(60))
                .await;
        }
        match next {
            Some(n) if n > from_seq => from_seq = n,
            _ => break,
        }
    }
    if std::env::var("BOOSTSEARCH_CLUSTER_DEBUG").is_ok() {
        eprintln!(
            "boostsearch: sent {sent} documents of [{index}][{shard}] to {} copies in term {term}",
            to.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shards_are_patched_for_a_document_and_for_a_bulk() {
        let mut acks = BTreeMap::new();
        acks.insert(
            "a".to_string(),
            Ack {
                total: 2,
                successful: 2,
                failed: 0,
                failures: vec![],
                manager_unreachable: false,
            },
        );
        acks.insert(
            "b".to_string(),
            Ack {
                total: 3,
                successful: 2,
                failed: 1,
                failures: vec![json!({"_node": "x"})],
                manager_unreachable: false,
            },
        );
        let mut one = json!({"_index": "a", "_id": "1", "_shards": {"total": 2, "successful": 1, "failed": 0}});
        patch_shards(&mut one, &acks);
        assert_eq!(one["_shards"]["successful"], 2);
        let mut bulk = json!({"items": [
            {"index": {"_index": "a", "_shards": {"total": 2, "successful": 1, "failed": 0}}},
            {"delete": {"_index": "b", "_shards": {"total": 3, "successful": 1, "failed": 0}}},
            {"index": {"_index": "c", "_shards": {"total": 1, "successful": 1, "failed": 0}}},
        ]});
        patch_shards(&mut bulk, &acks);
        assert_eq!(bulk["items"][0]["index"]["_shards"]["successful"], 2);
        assert_eq!(bulk["items"][1]["delete"]["_shards"]["failed"], 1);
        assert_eq!(bulk["items"][1]["delete"]["_shards"]["failures"][0]["_node"], "x");
        assert_eq!(bulk["items"][2]["index"]["_shards"]["successful"], 1);
    }

    #[test]
    fn targets_are_the_other_nodes_copies_and_initializing_ones_are_not_in_sync() {
        use crate::cluster::state::{ShardRouting, ShardState};
        let mut s = ClusterState::empty("c", "u");
        let mk = |node: &str, primary: bool, state: ShardState| ShardRouting {
            index: "i".into(),
            shard: 0,
            primary,
            state,
            node: Some(NodeId(node.into())),
            relocating_node: None,
            allocation_id: Some(node.into()),
            unassigned: None,
        };
        s.routing.indices.entry("i".into()).or_default().insert(
            0,
            vec![
                mk("p", true, ShardState::Started),
                mk("r1", false, ShardState::Started),
                mk("r2", false, ShardState::Initializing),
                mk("r3", false, ShardState::Unassigned),
            ],
        );
        let t = targets(&s, &NodeId("p".into()), "i", 0);
        assert_eq!(t, vec![(NodeId("r1".into()), true), (NodeId("r2".into()), false)]);
        // a copy is a copy of the index: any shard's write goes to every copy
        assert_eq!(targets(&s, &NodeId("p".into()), "i", 1), t);
    }

    #[test]
    fn the_global_checkpoint_is_what_every_in_sync_copy_has() {
        let mut t = Tracker::default();
        let (a, b) = (NodeId("a".into()), NodeId("b".into()));
        assert_eq!(t.global_checkpoint("i", 10, &[]), 10);
        t.acked("i", &a, 7);
        t.acked("i", &b, 9);
        t.acked("i", &a, 5);
        assert_eq!(t.local_checkpoint("i", &a), Some(7));
        assert_eq!(t.global_checkpoint("i", 10, &[a.clone(), b.clone()]), 7);
        assert_eq!(t.global_checkpoint("i", 10, std::slice::from_ref(&b)), 9);
        assert_eq!(t.global_checkpoint("j", 3, &[a]), 0);
    }
}
