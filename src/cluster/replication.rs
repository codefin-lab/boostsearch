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
pub fn targets(state: &ClusterState, me: &NodeId, index: &str, shard: u32) -> Vec<(NodeId, bool)> {
    state
        .routing
        .shards_of(index)
        .filter(|c| c.shard == shard && !c.primary)
        .filter(|c| {
            matches!(
                c.state,
                ShardState::Started | ShardState::Relocating | ShardState::Initializing
            )
        })
        .filter_map(|c| c.node.clone().map(|n| (n, c.state != ShardState::Initializing)))
        .filter(|(n, _)| n != me)
        .collect()
}

/// How many copies of an index took a request's writes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ack {
    pub total: usize,
    pub successful: usize,
    pub failed: usize,
    pub failures: Vec<Value>,
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
    if batches.is_empty() {
        return acks;
    }
    // every copy is written to at once; the answers are gathered in turn
    let mut waits = Vec::new();
    for ((node, index), (batch, in_sync)) in batches {
        let rt = rt.clone();
        let body = serde_json::to_vec(&json!({"index": index, "refresh": refresh, "ops": batch}))
            .unwrap_or_default();
        waits.push(tokio::spawn(async move {
            let answer =
                rt.call(&node, REPLICA_WRITE, body, std::time::Duration::from_secs(60)).await;
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
    for (node, index, in_sync, answer) in answers {
        let ack = acks.entry(index.clone()).or_default();
        let failure: Option<String> = match &answer {
            Some(e) if e.kind == Kind::Response => None,
            Some(e) => Some(String::from_utf8_lossy(&e.body).into_owned()),
            None => Some("no answer from the node".into()),
        };
        match failure {
            None => {
                if in_sync {
                    ack.successful += 1;
                }
            }
            Some(reason) => {
                if in_sync {
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
                        let _ = rt
                            .call(
                                mgr,
                                super::coordinator::SHARD_FAILED,
                                serde_json::to_vec(&body).unwrap_or_default(),
                                std::time::Duration::from_secs(10),
                            )
                            .await;
                    }
                }
            }
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

/// After a handler wrote: copy out, then say so in the answer.
pub async fn finish(
    response: axum::response::Response,
    ops: Vec<ReplicaOp>,
    refresh: &str,
) -> axum::response::Response {
    // nothing to copy to: the answer stands
    let any_target = super::with_state(|s| {
        let me = super::runtime().map(|r| r.local());
        let Some(me) = me else { return false };
        ops.iter().any(|op| !targets(s, &me, &op.index, op.shard).is_empty())
    });
    if !any_target {
        return response;
    }
    let acks = replicate(ops, refresh).await;
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

/// Fill a copy the manager placed here from the primary's documents.
pub async fn seed_replica(store: &Store, index: &str, shard: u32) -> Result<(), String> {
    let Some(rt) = super::runtime() else { return Ok(()) };
    let me = rt.local();
    let primary =
        super::with_state(|s| s.routing.primary(index, shard).and_then(|p| p.node.clone()));
    let Some(primary) = primary else {
        return Err(format!("[{index}][{shard}] has no primary to recover from"));
    };
    if primary == me {
        return Ok(());
    }
    let mut from_seq = 0u64;
    let mut tries = 0;
    loop {
        let body = serde_json::to_vec(
            &json!({"index": index, "shard": shard, "from_seq": from_seq, "size": 2000}),
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
                crate::api::doc::apply_replicated(&mut g, op);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shards_are_patched_for_a_document_and_for_a_bulk() {
        let mut acks = BTreeMap::new();
        acks.insert("a".to_string(), Ack { total: 2, successful: 2, failed: 0, failures: vec![] });
        acks.insert(
            "b".to_string(),
            Ack { total: 3, successful: 2, failed: 1, failures: vec![json!({"_node": "x"})] },
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
        assert!(targets(&s, &NodeId("p".into()), "i", 1).is_empty());
    }
}
