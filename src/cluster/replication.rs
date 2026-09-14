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
/// The files an index rewrites in place at every commit, each written whole:
/// they are not segments, and a recovery reads whatever they say now.
const REWRITTEN_IN_PLACE: &[&str] = &["_meta.json", "_versions.bin", "_terms.bin", "_docmeta.log"];

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
    /// The term the document itself was written in, where that is not
    /// `term`. A write carries one term, the primary's, and it is both. A
    /// recovery page or a resync carries documents written over many terms,
    /// under the term of the primary sending them -- which is what a copy
    /// checks for staleness -- and each document's own term is here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_term: Option<u64>,
}

tokio::task_local! {
    /// The writes the request in hand has made, waiting to be copied.
    pub static WRITES: Writes;
}

/// `BOOSTSEARCH_TRACE_WRITES`: every write, on every node, as it is copied,
/// taken, refused or answered for -- for following one document through a
/// chaos run, where the copies end up disagreeing about it.
fn trace_writes() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("BOOSTSEARCH_TRACE_WRITES").is_ok())
}

/// Where traced lines wait for the disk. A line written straight to stderr
/// per document slowed every node enough that the fault being traced -- a
/// matter of a tenth of a second -- stopped happening: twelve traced runs
/// lost nothing where four in twenty-six untraced ones had. Lines are
/// buffered and flushed every half second, and at shutdown.
static TRACE_OUT: std::sync::OnceLock<parking_lot::Mutex<std::io::BufWriter<std::io::Stderr>>> =
    std::sync::OnceLock::new();

pub(crate) fn trace_line(line: String) {
    use std::io::Write;
    let out = TRACE_OUT.get_or_init(|| {
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(500));
                flush_trace();
            }
        });
        parking_lot::Mutex::new(std::io::BufWriter::with_capacity(1 << 20, std::io::stderr()))
    });
    let _ = writeln!(out.lock(), "{} {line}", super::clock().wall());
}

/// Push out whatever traced lines are waiting.
pub fn flush_trace() {
    use std::io::Write;
    if let Some(out) = TRACE_OUT.get() {
        let _ = out.lock().flush();
    }
}

macro_rules! trace {
    ($($arg:tt)*) => { trace_line(format!($($arg)*)) };
}

/// The writes a request has made, shared between the request and the guard
/// that copies them if the request is dropped before it can.
pub type Writes = Arc<parking_lot::Mutex<Vec<ReplicaOp>>>;

/// Note a write the primary made, if a request is being handled.
///
/// A write made where no request's scope reaches -- a task spawned off the
/// request, a blocking thread -- was not noted, and so was never copied: the
/// primary held a document its copies never heard of, and nothing anywhere
/// said so. A node of a cluster now says so, with where it came from when
/// writes are traced.
pub fn record(op: ReplicaOp) {
    let Err(op) = WRITES.try_with(|w| w.lock().push(op.clone())).map_err(|_| op) else {
        return;
    };
    let clustered = super::runtime().is_some() && super::with_state(|s| s.nodes.len() > 1);
    if !clustered {
        return;
    }
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| {
        tracing::error!(
            "a write to [{}] was made outside any request and will not be copied",
            op.index
        );
    });
    if trace_writes() {
        trace!(
            "TRACE unrecorded {}/{} seq={} term={} from:\n{}",
            op.index,
            op.id,
            op.seq,
            op.term,
            std::backtrace::Backtrace::force_capture()
        );
    }
}

/// What a request wrote, copied out even if the request is not there to do
/// it.
///
/// A request's future is dropped where it stands when its caller hangs up.
/// The writes it had made were held in the request's own task, and went with
/// it: a primary stopped for eight seconds between writing a document and
/// finishing the request came back to a caller that had given up, and the
/// document stayed on the primary alone -- written, never copied, never
/// traced, and the copy never failed for missing it. Nothing changed primary,
/// so nothing resynced, and the copies disagreed for as long as the index
/// lived. The writes are now held where this guard can reach them too: a
/// guard dropped before they were handed on copies them itself, as the
/// reference's replication finishes whether or not anyone is waiting.
pub struct WritesGuard {
    writes: Writes,
    refresh: String,
    handed_on: bool,
}

impl WritesGuard {
    pub fn new(writes: Writes, refresh: String) -> Self {
        WritesGuard { writes, refresh, handed_on: false }
    }

    /// The writes, for the request to copy itself; the guard lets go of them.
    pub fn take(&mut self) -> Vec<ReplicaOp> {
        self.handed_on = true;
        std::mem::take(&mut *self.writes.lock())
    }
}

impl Drop for WritesGuard {
    fn drop(&mut self) {
        if self.handed_on {
            return;
        }
        let ops = std::mem::take(&mut *self.writes.lock());
        if ops.is_empty() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::error!(
                "{} writes of a dropped request could not be copied: no runtime to copy them on",
                ops.len()
            );
            return;
        };
        if trace_writes() {
            for op in &ops {
                trace!(
                    "TRACE dropped {}/{} seq={} term={} copied by the guard",
                    op.index, op.id, op.seq, op.term
                );
            }
        }
        let refresh = std::mem::take(&mut self.refresh);
        handle.spawn(async move {
            let _ = replicate(ops, &refresh).await;
        });
    }
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
    /// every node whose copy took this write. A write refused afterwards --
    /// because this node turns out not to be the primary -- has left the
    /// documents on all of them, not only here.
    pub wrote_to: Vec<NodeId>,
}

/// The shards a batch of writes went to, each named once: a bulk carries a
/// hundred documents into one shard, and the copy is failed for the shard.
fn shards_of_written(written: &[(String, u32, u64)]) -> Vec<(String, u32)> {
    let mut seen: Vec<(String, u32)> = Vec::new();
    for (index, shard, _) in written {
        let one = (index.clone(), *shard);
        if !seen.contains(&one) {
            seen.push(one);
        }
    }
    seen
}

/// Every copy this write reached, reported to the manager as no good.
///
/// A node that takes a write believing itself the primary writes the
/// documents down before it finds out otherwise, and the caller is told the
/// write did not happen. The documents stay. Nothing took them away again:
/// a resync trims a copy the new primary can see, and this copy was not in
/// the set; a fill replaces a copy from the primary, and this copy was the
/// one others were filled *from*. A copy filled from it inherited a document
/// nobody had acknowledged, and the two copies answered one search
/// differently for as long as the index lived -- once in about eighty chaos
/// runs. The reference fails a primary that finds a newer term, and so does
/// this: the manager takes those copies out of the in-sync set and they are
/// filled again from the primary that really is one.
///
/// Failing only this node's own copy was not enough, and the gate found it
/// out: the write had already been copied to the other nodes before the
/// refusal, and the document survived on one of *them*. So every copy the
/// write reached is named here, and the manager decides what to do with each.
///
/// This does not close the hole, and the ledger says so rather than the code
/// implying otherwise. The manager will not fail the copy it now calls the
/// primary -- it is what every other copy is filled from, and failing it on
/// the word of a node that did not know it was not one is how a cluster
/// loses acknowledged writes. So when the stray write reached the node that
/// is promoted next, its documents stay there and the other copies never get
/// them: two copies, one search, two answers. The gate reproduces it in
/// about one run in fifty.
///
/// What closes it is what the reference does, and it is not another guard at
/// this call site: a primary that takes over trims the operations it holds
/// from a term that was not its own and that nobody acknowledged. That needs
/// the term kept per operation on a copy and a trim at promotion, which is a
/// mechanism this does not have yet.
async fn fail_copies_written(index: &str, shard: u32, why: &str, also: &[NodeId]) {
    let Some(rt) = super::runtime() else { return };
    let me = rt.local();
    let state = rt.state();
    // Every copy the write reached is named, this node's own included, and
    // none of them is judged here. Leaving out "the primary" was tried and
    // was worse than useless: the state this node reads is the state that
    // just turned out to be out of date, and it still calls *this* node the
    // primary -- so the one copy certain to be holding the stray documents
    // was the one filtered out, and the report went out empty. The manager
    // knows which copy it now calls the primary and refuses to fail that one;
    // that is the right place for the rule, and the only place that knows.
    let touched: Vec<NodeId> = std::iter::once(me.clone()).chain(also.iter().cloned()).collect();
    // one report per node touched, naming the node: the allocation id this
    // node remembers is the one thing it cannot be trusted about, and the
    // manager resolves the node against its own table. The id is sent too,
    // for a manager that has nothing placed on that node any more.
    let mine: Vec<(NodeId, String)> = touched
        .iter()
        .map(|n| {
            let aid = state
                .routing
                .shards_of(index)
                .find(|c| c.shard == shard && c.node.as_ref() == Some(n))
                .and_then(|c| c.allocation_id.clone())
                .unwrap_or_default();
            (n.clone(), aid)
        })
        .collect();
    if std::env::var("BOOSTSEARCH_CLUSTER_DEBUG").is_ok() {
        eprintln!(
            "boostsearch: {} failing the copies a refused write reached ({why}): {}",
            super::clock().wall(),
            mine.iter()
                .map(|(n, a)| format!("{}={}", n.as_str(), if a.is_empty() { "?" } else { a }))
                .collect::<Vec<_>>()
                .join(",")
        );
    }
    let body_for = |node: &NodeId, aid: &str| {
        json!({"index": index, "shard": shard, "allocation_id": aid, "node": node.as_str(),
               "message": format!("wrote as a primary it is not: {why}")})
    };
    let Some(mgr) = state.cluster_manager.clone() else {
        for (node, aid) in mine {
            retry_later(super::coordinator::SHARD_FAILED, &body_for(&node, &aid));
        }
        return;
    };
    for (node, aid) in mine {
        let body = body_for(&node, &aid);
        let answer = rt
            .call(
                &mgr,
                super::coordinator::SHARD_FAILED,
                serde_json::to_vec(&body).unwrap_or_default(),
                std::time::Duration::from_secs(10),
            )
            .await;
        if !matches!(answer, Some(ref a) if a.kind == Kind::Response) {
            retry_later(super::coordinator::SHARD_FAILED, &body);
        }
    }
}

/// A refusal that comes after this node has already written the documents
/// down. The caller is told the write did not happen; the documents are
/// here all the same, and if this node is not the primary any more nobody
/// will take them away again. One line per refusal, not per write.
fn note_refused_after_writing(why: &str, ids: &[String]) {
    if ids.is_empty() || std::env::var("BOOSTSEARCH_CLUSTER_DEBUG").is_err() {
        return;
    }
    eprintln!(
        "boostsearch: {} refused after writing ({why}): {}",
        super::clock().wall(),
        ids.iter().take(25).cloned().collect::<Vec<_>>().join(",")
    );
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
                {
                    let copies: Vec<(u32, String)> = state
                        .routing
                        .shards_of(&index)
                        .filter(|c| c.node.as_ref() == Some(&node) && !c.primary)
                        .filter_map(|c| c.allocation_id.clone().map(|a| (c.shard, a)))
                        .collect();
                    for (shard, aid) in copies {
                        let body = json!({"index": index, "shard": shard, "allocation_id": aid,
                            "message": format!("replication to [{}] failed: {reason}", node.as_str())});
                        let answer = match &manager {
                            Some(mgr) => {
                                rt.call(
                                    mgr,
                                    super::coordinator::SHARD_FAILED,
                                    serde_json::to_vec(&body).unwrap_or_default(),
                                    std::time::Duration::from_secs(10),
                                )
                                .await
                            }
                            // no manager to tell: the report waits for one
                            None => None,
                        };
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
                            retry_later(super::coordinator::SHARD_FAILED, &body);
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
    {
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
            // Every id in the set that did not take this write leaves it,
            // whether or not a copy is placed under it now. An id kept for a
            // copy whose node had left was taken for the cluster's memory of
            // where the data was -- but that copy is missing this write, and
            // when the primary was lost the manager put the primary back on
            // the node holding it, with none of the writes acknowledged while
            // it was away. A chaos run lost ninety acknowledged documents
            // from the surviving copies that way. The reference marks such a
            // copy stale before it acknowledges the write.
            for (shard, ids) in &m.in_sync_allocations {
                for id in ids {
                    if !fine.contains(id) {
                        let body = json!({"index": index, "shard": shard, "allocation_id": id});
                        let answer = match &manager {
                            Some(mgr) => {
                                rt.call(
                                    mgr,
                                    super::coordinator::SHARD_STALE,
                                    serde_json::to_vec(&body).unwrap_or_default(),
                                    std::time::Duration::from_secs(10),
                                )
                                .await
                            }
                            // no manager to tell: the report waits for one
                            None => None,
                        };
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
                            retry_later(super::coordinator::SHARD_STALE, &body);
                            manager_unreachable = true;
                        }
                    }
                }
            }
        }
    }
    if trace_writes() {
        for op in &ops {
            let took: Vec<&str> = acked_nodes
                .get(&op.index)
                .map(|v| v.iter().map(|n| n.as_str()).collect())
                .unwrap_or_default();
            trace!(
                "TRACE primary {} {}/{} seq={} term={} copies_took={:?} manager_unreachable={}",
                me.as_str(),
                op.index,
                op.id,
                op.seq,
                op.term,
                took,
                manager_unreachable
            );
        }
    }
    // successful copies of a shard: the primary and the in-sync replica
    // copies of that shard whose node answered; over a request that wrote
    // to several shards, the fewest
    for (index, ack) in acks.iter_mut() {
        let acked = acked_nodes.get(index).cloned().unwrap_or_default();
        ack.wrote_to = acked.clone();
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
/// Whether this node may still answer for these writes, asked at the moment
/// it answers: it has a manager whose word is current, it is the primary of
/// each shard the state now names, and the term it wrote under is that
/// shard's term. A request let through before the node was stopped, and
/// finished after it was let go on, was asked this only on the way in.
fn may_still_answer(written: &[(String, u32, u64)]) -> bool {
    if !super::has_manager() {
        return false;
    }
    let Some(me) = super::runtime().map(|r| r.local()) else { return true };
    super::with_state(|s| {
        written.iter().all(|(index, shard, term)| {
            let primary_here = s
                .routing
                .primary(index, *shard)
                .and_then(|p| p.node.as_ref())
                .map(|n| *n == me)
                .unwrap_or(false);
            let current =
                s.indices.get(index).and_then(|m| m.primary_terms.get(shard).copied()).unwrap_or(1);
            primary_here && *term >= current
        })
    })
}

fn no_longer_primary() -> axum::response::Response {
    crate::api::err(
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "unavailable_shards_exception",
        "this node is no longer the primary for this write, or cannot say that it is; not \
         acknowledged, retry",
    )
}

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
    let written: Vec<(String, u32, u64)> =
        ops.iter().map(|o| (o.index.clone(), o.shard, o.term)).collect();
    if nothing_to_do {
        // a node that is the whole cluster answers for itself; one that is
        // not asks again now, as it answers
        let in_a_cluster = super::with_state(|s| s.nodes.len() > 1);
        if in_a_cluster && !may_still_answer(&written) {
            if trace_writes() {
                for op in &ops {
                    trace!(
                        "TRACE answer {}/{} seq={} term={} refused no-longer-primary",
                        op.index, op.id, op.seq, op.term
                    );
                }
            }
            return no_longer_primary();
        }
        if trace_writes() {
            for op in &ops {
                trace!(
                    "TRACE answer {}/{} seq={} term={} alone status={}",
                    op.index,
                    op.id,
                    op.seq,
                    op.term,
                    response.status().as_u16()
                );
            }
        }
        return response;
    }
    // A node that has lost the cluster manager knows nothing of what the
    // cluster decided while it was away: the primary it thinks it holds may
    // be somebody else's now, and a write it acknowledged alone would be
    // thrown away. OpenSearch blocks writes the same way, on the
    // `no cluster-manager` block its checks raise.
    //
    // Asked *after* whether there is anything to copy to, because there is
    // nothing to be wrong about otherwise: a node that is the whole cluster
    // used to refuse every write for the tenth of a second between its
    // listener opening and its electing itself, which is where anything that
    // starts a node and writes at once lives -- the benchmark found it.
    //
    // The write is refused, but not before it is copied. The node had a
    // manager when the request came in -- `run_with_replication` refuses one
    // that has none before the handler runs -- and lost it while the handler
    // wrote: a manager stopped for eight seconds came back to find itself
    // voted out, and every write it had in hand was already on its own copy.
    // Answering from here left each one there alone: never copied, never
    // traced, the copy never failed for missing it, and the primary stayed
    // the primary once it was voted back in. The writes go to the copies as
    // any others do, a copy that does not take them is reported when there
    // is a manager to hear it, and the caller is told the write was not
    // acknowledged.
    if !super::has_manager() {
        if trace_writes() {
            for op in &ops {
                trace!(
                    "TRACE answer {}/{} seq={} term={} refused no-cluster-manager",
                    op.index, op.id, op.seq, op.term
                );
            }
        }
        let _ = replicate(ops, refresh).await;
        return crate::api::err(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "cluster_block_exception",
            "blocked by: [SERVICE_UNAVAILABLE/2/no cluster-manager];",
        );
    }
    let traced: Vec<String> = if trace_writes() {
        ops.iter().map(|o| format!("{}/{} seq={} term={}", o.index, o.id, o.seq, o.term)).collect()
    } else {
        Vec::new()
    };
    // what this node has already written down when a refusal below decides the
    // write did not happen: the document stays here whatever the caller is
    // told, so the ids are worth a line of the cluster's own notes
    let written_ids: Vec<String> =
        ops.iter().map(|o| format!("{}/{}@{}", o.index, o.id, o.seq)).collect();
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
        for t in &traced {
            trace!("TRACE answer {t} refused stale-term");
        }
        note_refused_after_writing("stale-term", &written_ids);
        for (index, shard) in shards_of_written(&written) {
            let reached = acks.get(&index).map(|a| a.wrote_to.clone()).unwrap_or_default();
            fail_copies_written(&index, shard, "a copy refused its term", &reached).await;
        }
        return crate::api::err(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "unavailable_shards_exception",
            "the primary that took this write is no longer the primary (its term is stale); retry",
        );
    }
    if acks.values().any(|a| a.manager_unreachable) {
        for t in &traced {
            trace!("TRACE answer {t} refused manager-unreachable");
        }
        note_refused_after_writing("manager-unreachable", &written_ids);
        return crate::api::err(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "unavailable_shards_exception",
            "a copy did not take this write and the cluster manager could not be told; not acknowledged, retry",
        );
    }
    if !may_still_answer(&written) {
        for t in &traced {
            trace!("TRACE answer {t} refused no-longer-primary");
        }
        note_refused_after_writing("no-longer-primary", &written_ids);
        for (index, shard) in shards_of_written(&written) {
            let reached = acks.get(&index).map(|a| a.wrote_to.clone()).unwrap_or_default();
            fail_copies_written(&index, shard, "no longer the primary", &reached).await;
        }
        return no_longer_primary();
    }
    for t in &traced {
        trace!("TRACE answer {t} status={}", response.status().as_u16());
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
                if trace_writes() {
                    let refused = ops.iter().any(|op| op.term < known_term);
                    for op in &ops {
                        trace!(
                            "TRACE replica {} {index}/{} seq={} term={} known_term={known_term} {}",
                            from.as_str(),
                            op.id,
                            op.seq,
                            op.term,
                            if refused { "refused" } else { "taken" }
                        );
                    }
                }
                if ops.iter().any(|op| op.term < known_term) {
                    return e.error(
                        from,
                        &format!(
                            "stale primary term for [{index}]: {} < {known_term}",
                            ops.iter().map(|o| o.term).min().unwrap_or(0)
                        ),
                    );
                }
                // the start and end of a new primary's resync
                if let Some(rs) = v.get("resync") {
                    let term = rs.get("term").and_then(|t| t.as_u64()).unwrap_or(0);
                    if term < known_term {
                        return e.error(
                            from,
                            &format!("stale primary term for [{index}]: {term} < {known_term}"),
                        );
                    }
                    let phase = rs.get("phase").and_then(|p| p.as_str()).unwrap_or("");
                    if phase == "begin" {
                        // a copy being filled is filled from the new primary
                        // itself, and matches it without being trimmed
                        if !arrived().lock().contains_key(&index) {
                            open_resyncs()
                                .lock()
                                .insert(index.clone(), (term, std::collections::HashSet::new()));
                        }
                        return e.response(from, b"{}".to_vec());
                    }
                    if phase == "end" {
                        let seen = {
                            let mut open = open_resyncs().lock();
                            match open.get(&index) {
                                Some((t, _)) if *t == term => open.remove(&index).map(|(_, s)| s),
                                _ => None,
                            }
                        };
                        let Some(seen) = seen else {
                            return e.response(from, b"{}".to_vec());
                        };
                        let store = store.clone();
                        let name = index.clone();
                        let trimmed = tokio::task::spawn_blocking(move || {
                            let Some(st) = store.get(&name) else { return Ok(0usize) };
                            let mut g = st.write();
                            let (held, _) =
                                crate::api::doc::scan_replicated(&g, u32::MAX, 0, usize::MAX);
                            let doomed: Vec<String> = held
                                .into_iter()
                                .filter(|o| o.source.is_some() && !seen.contains(&o.id))
                                .map(|o| o.id)
                                .collect();
                            let first = g.seq_no + 1;
                            for (seq, id) in (first..).zip(doomed.iter()) {
                                let op = ReplicaOp {
                                    index: name.clone(),
                                    id: id.clone(),
                                    routing: None,
                                    version: g.version_of(id) + 1,
                                    seq,
                                    term,
                                    shard: g.shard_of_doc(id) as u32,
                                    source: None,
                                    doc_term: None,
                                };
                                if trace_writes() {
                                    trace!("TRACE trimmed {name}/{id} term={term}");
                                }
                                crate::api::doc::apply_replicated(&mut g, &op);
                            }
                            g.sync_translog()?;
                            let _ = g.refresh();
                            Ok::<usize, String>(doomed.len())
                        })
                        .await
                        .unwrap_or_else(|e| Err(format!("resync trim panicked: {e}")));
                        return match trimmed {
                            Ok(n) => e.response(
                                from,
                                serde_json::to_vec(&json!({"trimmed": n})).unwrap_or_default(),
                            ),
                            Err(msg) => e.error(from, &msg),
                        };
                    }
                }
                seen_by_resync(&index, &ops);
                // a copy being filled takes the write when the seed is done
                if park(&index, &ops) {
                    if trace_writes() {
                        for op in &ops {
                            trace!("TRACE replica parked {index}/{} seq={}", op.id, op.seq);
                        }
                    }
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
                    g.sync_translog()?;
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
                // whether this node's writes go to the copy asking: until they
                // do, a page that ends the scan is not the end of what it needs
                let routes_here = v.get("for_node").and_then(|n| n.as_str()).map(|asking| {
                    let state = super::current_state();
                    targets(&state, &from, &index, shard).iter().any(|(n, _)| n.as_str() == asking)
                });
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
                        serde_json::to_vec(
                            &json!({"ops": ops, "next_seq": next, "routes_here": routes_here}),
                        )
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
                        // The listing is taken at one moment and the files
                        // are fetched one by one afterwards, with nothing
                        // holding the commit open in between: a write and a
                        // refresh in the middle rewrites `_meta.json` and
                        // replaces segments, and the copy is assembled out of
                        // two generations -- short of documents, at a
                        // sequence number the catch-up will never revisit,
                        // and reported as in sync. What each file looked like
                        // is carried with it, and a fetch of a file that has
                        // changed since is refused.
                        let meta = entry.metadata().ok();
                        let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                        let stamp = meta
                            .as_ref()
                            .and_then(|m| m.modified().ok())
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_nanos() as u64)
                            .unwrap_or(0);
                        files.push(json!({"name": name, "len": len, "stamp": stamp}));
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
                    let at = dir.join(&name);
                    let held = std::fs::metadata(&at).map_err(|e| e.to_string())?;
                    let now_stamp = held
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(0);
                    // A segment file never changes once it is written, so one
                    // that has is a commit that moved under the recovery and
                    // the copy would be assembled out of two generations.
                    // The index's own bookkeeping is different: `_meta.json`
                    // and the version map are rewritten in place on every
                    // commit, and each is written whole and atomically, so
                    // whatever is there now is consistent by itself. Holding
                    // those to the listing failed every recovery of an index
                    // that was taking writes.
                    let rewritten = REWRITTEN_IN_PLACE.contains(&name.as_str());
                    let whole = v.get("whole").and_then(|i| i.as_u64());
                    let stamp = v.get("stamp").and_then(|i| i.as_u64());
                    if !rewritten
                        && (whole.map(|w| w != held.len()).unwrap_or(false)
                            || stamp.map(|s| s != now_stamp).unwrap_or(false))
                    {
                        return Err(format!(
                            "[{name}] changed since the listing: the commit moved under the \
                             recovery"
                        ));
                    }
                    let mut f = std::fs::File::open(&at).map_err(|e| e.to_string())?;
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
    let files: Vec<(String, u64, u64)> = v
        .get("files")
        .and_then(|f| f.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|f| {
                    Some((
                        f.get("name")?.as_str()?.to_string(),
                        f.get("len")?.as_u64()?,
                        f.get("stamp").and_then(|s| s.as_u64()).unwrap_or(0),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let Some(dest) = store.index_dir(index) else { return Ok(false) };
    let tmp = dest.with_extension("recovering");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    // the files an index rewrites in place go last: the segments they
    // describe are fetched first, so what these say is true of what is there
    let mut files = files;
    files.sort_by_key(|(name, _, _)| REWRITTEN_IN_PLACE.contains(&name.as_str()));
    for (name, len, stamp) in &files {
        let mut offset = 0u64;
        let mut out = std::fs::File::create(tmp.join(name)).map_err(|e| e.to_string())?;
        use std::io::Write;
        while offset < *len {
            let ask = serde_json::to_vec(&json!({
                "index": index,
                "name": name,
                "offset": offset,
                "len": CHUNK,
                // what this file was when it was listed: the far side refuses
                // to serve it if it is not that any more
                "whole": len,
                "stamp": stamp,
            }))
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
    // The copy that was here goes, translog and all, and the files take its
    // place.
    //
    // Its translog used to be replayed over them, as though it held writes
    // copied in while the files travelled. It did not: those wait for the
    // fill beside it (`park`) and go in when it ends, and what the primary
    // took after the commit these files are comes from the scan that
    // follows. What the old translog held was the old copy -- writes the
    // primary refused or never had among them -- and it came back, under
    // term one, over the primary's own: a filled copy held twenty-three
    // documents its primary did not, and kept them.
    let store2 = store.clone();
    let name = index.to_string();
    let tmp2 = tmp.clone();
    let adopted = tokio::task::spawn_blocking(move || -> Result<(), String> {
        store2.adopt(&name, &tmp2).map_err(|e| e.to_string())?;
        let Some(st) = store2.get(&name) else {
            return Err(format!("[{name}] did not open after recovery"));
        };
        let _ = st.write().refresh();
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(format!("adopting panicked: {e}")));
    adopted?;
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

/// Resyncs a new primary has opened on this copy: the term, and every
/// document a write of that term or later has touched here since.
///
/// A primary that takes over sends the copies everything it holds, and a
/// copy then held both that and whatever the old primary gave it and the new
/// one never had -- writes nobody was told were taken, left on one copy and
/// not the other, so the two answered one search differently for as long as
/// the index lived. At the end of the resync, what no write of the new term
/// touched is what the new primary does not have, and it goes.
type OpenResyncs = parking_lot::Mutex<BTreeMap<String, (u64, std::collections::HashSet<String>)>>;

fn open_resyncs() -> &'static OpenResyncs {
    static OPEN: std::sync::OnceLock<OpenResyncs> = std::sync::OnceLock::new();
    OPEN.get_or_init(|| parking_lot::Mutex::new(BTreeMap::new()))
}

/// Note the documents these writes touch, if a resync is open on the index.
fn seen_by_resync(index: &str, ops: &[ReplicaOp]) {
    let mut open = open_resyncs().lock();
    if let Some((term, seen)) = open.get_mut(index) {
        for op in ops.iter().filter(|op| op.term >= *term) {
            seen.insert(op.id.clone());
        }
    }
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
    // a copy filled now matches the primary it is filled from; a resync
    // opened before would trim what the fill brought in
    open_resyncs().lock().remove(index);
    let notes = std::env::var("BOOSTSEARCH_CLUSTER_DEBUG").is_ok();
    // whether there was anything here before this recovery: a half-filled copy
    // this recovery made is not something to leave behind
    let held_before = store.get(index).is_some();
    let before = store.get(index).map(|st| st.read().live_ids.len()).unwrap_or(0);
    let r = seed_replica_inner(store, index, shard, primary).await;
    let r = match r {
        Ok(()) => apply_what_waited(store, index).await,
        Err(why) => {
            // What the primary sent while this was filling was answered
            // `applied` and parked; it is dropped here. That is only safe
            // because a failed copy is reported `Failed` and is no longer
            // in sync -- but a half-filled index left in the store is read
            // from like any other, and answers a search with a part of the
            // documents. It goes with the recovery that made it.
            let parked = arrived().lock().remove(index).map(|v| v.len()).unwrap_or(0);
            if !held_before && store.get(index).is_some() {
                store.drop_local(index);
            }
            tracing::warn!(
                "filling [{index}] failed ({why}); {parked} writes that waited for it were \
                 dropped and the copy is reported failed"
            );
            Err(why)
        }
    };
    if r.is_ok() {
        *done = Some(allocation_id.to_string());
    }
    if notes {
        let after = store.get(index).map(|st| st.read().live_ids.len()).unwrap_or(0);
        // the sequence number the copy stands at once filled, and the primary's
        // at the moment it was asked: a copy that stands above documents it
        // does not hold is a copy the next catch-up will walk past them
        let at_seq = store.get(index).map(|st| st.read().seq_no).unwrap_or(0);
        eprintln!(
            "boostsearch: {} filled [{index}] as {allocation_id} from {}: {before} documents here before, {after} after, seq_no {at_seq} ({})",
            super::clock().wall(),
            primary.as_str(),
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
        // what the fill let in behind the pages it copied: written down
        // because a copy that ends a chaos run holding one document more than
        // its primary was filled in that run, and this is the only door a
        // document comes through after the copying and before the copy is a
        // copy. Cheap enough to leave on with the cluster's own notes: one
        // line for a recovery, not one for a write.
        let notes = std::env::var("BOOSTSEARCH_CLUSTER_DEBUG").is_ok();
        let mut waited: Vec<String> = Vec::new();
        loop {
            let batch = {
                let mut m = arrived().lock();
                match m.get_mut(&name) {
                    Some(waiting) if !waiting.is_empty() => std::mem::take(waiting),
                    // nothing waiting: close the recovery while the lock is held
                    _ => {
                        m.remove(&name);
                        if notes && !waited.is_empty() {
                            eprintln!(
                                "boostsearch: {} [{name}]: {} writes waited for the fill and went \
                                 in after it: {}",
                                super::clock().wall(),
                                waited.len(),
                                waited.iter().take(25).cloned().collect::<Vec<_>>().join(",")
                            );
                        }
                        return Ok(());
                    }
                }
            };
            if notes {
                waited.extend(batch.iter().map(|op| {
                    format!("{}@{}{}", op.id, op.seq, if op.source.is_none() { "-del" } else { "" })
                }));
            }
            let Some(st) = store.get(&name) else {
                arrived().lock().remove(&name);
                return Err(format!("no copy of [{name}] here to finish"));
            };
            let mut g = st.write();
            for op in &batch {
                crate::api::doc::apply_replicated(&mut g, op);
            }
            g.sync_translog()?;
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
    let mut pages = 0usize;
    let mut applied_total = 0usize;
    let notes = std::env::var("BOOSTSEARCH_CLUSTER_DEBUG").is_ok();
    // A copy the manager has just placed is known to the primary only once
    // the primary has taken the publication that placed it; a write it took
    // before then went to the copies it knew, not to this one, and if the
    // scan had already passed where that write stands the copy never had it.
    // One chaos run left a freshly filled copy twenty acknowledged documents
    // short this way. The scan ends only on a page answered by a primary that
    // already sends its writes here: everything it took before that is in the
    // page, and everything after comes as a write.
    let waiting_since = std::time::Instant::now();
    loop {
        let body = serde_json::to_vec(&json!({
            "index": index, "shard": u32::MAX, "from_seq": from_seq, "size": 2000,
            "for_node": me.as_str(),
        }))
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
        // a primary from before this was asked answers without it
        let routes_here = v.get("routes_here").and_then(|r| r.as_bool()).unwrap_or(true);
        let store = store.clone();
        let name = index.to_string();
        let applied = tokio::task::spawn_blocking(move || {
            let Some(st) = store.get(&name) else {
                return Err(format!("no copy of [{name}] here"));
            };
            let mut g = st.write();
            for op in &ops {
                if trace_writes() {
                    trace!(
                        "TRACE recovered {name}/{} seq={} term={} overwrite={overwrite}",
                        op.id, op.seq, op.term
                    );
                }
                if overwrite {
                    crate::api::doc::apply_recovered(&mut g, op);
                } else {
                    crate::api::doc::apply_replicated(&mut g, op);
                }
            }
            g.sync_translog()?;
            Ok(())
        })
        .await
        .unwrap_or_else(|e| Err(format!("recovery apply panicked: {e}")));
        applied?;
        pages += 1;
        applied_total += v.get("ops").and_then(|o| o.as_array()).map(|a| a.len()).unwrap_or(0);
        match next {
            Some(n) if n > from_seq => from_seq = n,
            _ if routes_here => break,
            _ if waiting_since.elapsed() > std::time::Duration::from_secs(30) => {
                tracing::warn!(
                    "the primary of [{index}] did not start sending its writes here within \
                     thirty seconds of the scan ending; the copy is taken as filled"
                );
                break;
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
        }
    }
    if notes {
        eprintln!(
            "boostsearch: {} caught [{index}] up from {} starting at seq {from}: {applied_total} documents in {pages} pages, stopped at seq {from_seq}",
            super::clock().wall(),
            primary.as_str()
        );
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
/// copy. Documents the copy has and this node does not are trimmed when the
/// resync ends: a copy in sync is never short of an acknowledged write, so
/// what this node lacks was never acknowledged.
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
    // the copies that did not take everything: they are failed as they are
    // found, and nothing further is sent to them
    let mut missed: Vec<NodeId> = Vec::new();
    let mark = |phase: &str| {
        serde_json::to_vec(&json!({"index": index, "refresh": "", "ops": [],
            "resync": {"phase": phase, "term": term}}))
        .unwrap_or_default()
    };
    for node in to {
        let _ =
            rt.call(node, REPLICA_WRITE, mark("begin"), std::time::Duration::from_secs(30)).await;
    }
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
        let reachable: Vec<NodeId> = to.iter().filter(|n| !missed.contains(n)).cloned().collect();
        for node in &reachable {
            // a copy that did not take a page of the resync is a copy that is
            // missing writes the new primary has. Saying nothing left it in
            // the in-sync set, where it could later be handed the primary and
            // those writes would be gone for good.
            let answer = rt
                .call(node, REPLICA_WRITE, body.clone(), std::time::Duration::from_secs(60))
                .await;
            if !matches!(answer, Some(ref a) if a.kind == Kind::Response) {
                let why = match &answer {
                    Some(a) => String::from_utf8_lossy(&a.body).into_owned(),
                    None => "no answer".to_string(),
                };
                let told =
                    fail_copy(store, index, shard, node, &format!("resync failed: {why}")).await;
                if !told {
                    tracing::error!(
                        "[{index}][{shard}]: a copy on {node} missed the resync and the manager \
                         could not be told; it is still in the in-sync set"
                    );
                }
                missed.push(node.clone());
            }
        }
        match next {
            Some(n) if n > from_seq => from_seq = n,
            _ => break,
        }
    }
    // every copy that took the whole resync lets go of what it has and the
    // new primary does not
    for node in to.iter().filter(|n| !missed.contains(n)) {
        let _ = rt.call(node, REPLICA_WRITE, mark("end"), std::time::Duration::from_secs(60)).await;
    }
    if std::env::var("BOOSTSEARCH_CLUSTER_DEBUG").is_ok() {
        eprintln!(
            "boostsearch: sent {sent} documents of [{index}][{shard}] to {} copies in term {term}, the primary at seq {}",
            to.len(),
            store.get(index).map(|st| st.read().seq_no).unwrap_or(0)
        );
    }
    if !missed.is_empty() {
        return Err(format!(
            "the resync of [{index}][{shard}] did not reach {} of {} copies",
            missed.len(),
            to.len()
        ));
    }
    Ok(())
}

/// Tell the manager a copy on this node's peer is no good, so it is taken out
/// of the in-sync set and filled again.
async fn fail_copy(store: &Store, index: &str, shard: u32, node: &NodeId, why: &str) -> bool {
    let _ = store;
    let Some(rt) = super::runtime() else { return false };
    let state = rt.state();
    let Some(mgr) = state.cluster_manager.clone() else { return false };
    let ids: Vec<String> = state
        .routing
        .shards_of(index)
        .filter(|c| c.shard == shard && c.node.as_ref() == Some(node) && !c.primary)
        .filter_map(|c| c.allocation_id.clone())
        .collect();
    let mut told = true;
    for aid in ids {
        let body = json!({"index": index, "shard": shard, "allocation_id": aid, "message": why});
        // the manager hearing this is what takes the copy out of the in-sync
        // set. If it does not hear it, the copy stays eligible to be handed
        // the primary while missing writes -- so it is tried again, and the
        // caller is told when it never landed.
        let mut landed = false;
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            let answer = rt
                .call(
                    &mgr,
                    super::coordinator::SHARD_FAILED,
                    serde_json::to_vec(&body).unwrap_or_default(),
                    std::time::Duration::from_secs(10),
                )
                .await;
            if matches!(answer, Some(ref a) if a.kind == Kind::Response) {
                landed = true;
                break;
            }
        }
        if !landed {
            retry_later(super::coordinator::SHARD_FAILED, &body);
        }
        told &= landed;
    }
    told
}

/// Reports to the manager -- a copy failed, a copy stale -- that a write
/// needed and could not deliver.
///
/// A primary whose copy did not take a write tells the manager, so the copy
/// leaves the in-sync set and is filled again; when the manager could not be
/// reached, the write was refused and the report dropped. The primary kept
/// the document, the copy stayed in the set without it, and nothing came
/// back to either: the two answered one search differently for as long as
/// the index lived. The reference does not let go of a failure it could not
/// record. The report now waits here and is sent again until the manager
/// takes it -- or until it is nobody's business: this node no longer the
/// primary (the new one resyncs), or the copy gone from the routing and the
/// in-sync set both.
type PendingReports = parking_lot::Mutex<Vec<(&'static str, Value)>>;

fn pending_reports() -> &'static PendingReports {
    static PENDING: std::sync::OnceLock<PendingReports> = std::sync::OnceLock::new();
    PENDING.get_or_init(|| parking_lot::Mutex::new(Vec::new()))
}

fn retry_later(action: &'static str, body: &Value) {
    {
        let mut pending = pending_reports().lock();
        let same = |b: &Value| {
            b.get("index") == body.get("index")
                && b.get("allocation_id") == body.get("allocation_id")
        };
        if pending.iter().any(|(a, b)| *a == action && same(b)) {
            return;
        }
        pending.push((action, body.clone()));
    }
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        tokio::spawn(retry_reports());
    });
}

async fn retry_reports() {
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let batch: Vec<(&'static str, Value)> = pending_reports().lock().clone();
        if batch.is_empty() {
            continue;
        }
        let Some(rt) = super::runtime() else { continue };
        let me = rt.local();
        let state = rt.state();
        let Some(mgr) = state.cluster_manager.clone() else { continue };
        for (action, body) in batch {
            let index = body.get("index").and_then(|v| v.as_str()).unwrap_or("");
            let shard = body.get("shard").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let aid = body.get("allocation_id").and_then(|v| v.as_str()).unwrap_or("");
            let primary_here =
                state.routing.primary(index, shard).and_then(|p| p.node.clone()).as_ref()
                    == Some(&me);
            let placed = state
                .routing
                .shards_of(index)
                .any(|c| c.shard == shard && c.allocation_id.as_deref() == Some(aid));
            let in_sync = state
                .indices
                .get(index)
                .and_then(|m| m.in_sync_allocations.get(&shard))
                .map(|ids| ids.iter().any(|i| i == aid))
                .unwrap_or(false);
            let settled = if !primary_here || !(placed || in_sync) {
                true
            } else {
                let answer = rt
                    .call(
                        &mgr,
                        action,
                        serde_json::to_vec(&body).unwrap_or_default(),
                        std::time::Duration::from_secs(10),
                    )
                    .await;
                matches!(answer, Some(ref a) if a.kind == Kind::Response)
            };
            if settled {
                if trace_writes() {
                    trace!("TRACE report settled {action} {index}/{aid}");
                }
                pending_reports().lock().retain(|(a, b)| !(*a == action && *b == body));
            }
        }
    }
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
                wrote_to: vec![],
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
                wrote_to: vec![],
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
