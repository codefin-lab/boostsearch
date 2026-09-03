//! The data path as the simulation runs it: a node that is the coordinator
//! and a replicated store, with the store's rules but none of its I/O. A
//! client writes documents to whichever node; the node holding the
//! primary gives each write its sequence number and term, applies it,
//! copies it to every copy, and answers once the in-sync copies have
//! taken it; a copy that does not answer in time is reported failed; a
//! copy the manager places is filled from the primary by a scan; a copy
//! the manager no longer places here is dropped; what a node wrote is on
//! its disk across a crash.
//!
//! What the simulation holds this to (the tests at the bottom): nothing
//! acknowledged is lost, no two nodes accept writes as the primary of one
//! index in one term, and no two copies of an index differ once the
//! cluster is quiet.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::clock::{Clock, Millis};
use super::coordinator::{Coordinator, SHARD_FAILED};
use super::metadata::{MapSource, MetadataSource, ShardHost};
use super::sim::{Durable, Input, NodeLogic, Output};
use super::state::{ClusterState, IndexMetadata, ShardRouting, ShardState};
use super::transport::{Envelope, Kind, NodeId};

pub const CLIENT_WRITE: &str = "client:write";
pub const COPY_WRITE: &str = "model:copy/write";
pub const SCAN: &str = "model:recovery/scan";
const D_DOCS: &str = "model_docs";
const T_TICK: u64 = 100;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDoc {
    pub version: u64,
    pub seq: u64,
    pub term: u64,
    pub value: u64,
}

pub type Docs = BTreeMap<String, BTreeMap<String, ModelDoc>>;

/// A write accepted by a node as the primary: what the single-writer
/// check reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    pub index: String,
    pub term: u64,
    pub seq: u64,
    pub id: String,
    pub value: u64,
}

struct PendingWrite {
    client: NodeId,
    client_rid: u64,
    index: String,
    id: String,
    seq: u64,
    term: u64,
    waiting: BTreeSet<NodeId>,
    in_sync_waiting: BTreeSet<NodeId>,
    acked: bool,
    deadline: Millis,
}

struct Recovery {
    index: String,
    allocation_id: String,
    from: NodeId,
    next_seq: u64,
}

/// What the coordinator's host and metadata source share with the model.
struct Shared {
    docs: parking_lot::Mutex<Docs>,
    /// the allocation id of each copy here
    alloc: parking_lot::Mutex<BTreeMap<String, String>>,
    /// copies the coordinator asked the host to start, waiting for the model
    to_start: parking_lot::Mutex<Vec<(IndexMetadata, ShardRouting)>>,
    /// the index metadata every node reads (the cluster's, held in one map)
    source: Arc<MapSource>,
}

struct ModelSource(Arc<Shared>);

impl MetadataSource for ModelSource {
    fn snapshot(&self) -> BTreeMap<String, IndexMetadata> {
        self.0.source.snapshot()
    }
    fn tombstones(&self) -> Vec<Value> {
        self.0.source.tombstones()
    }
    fn has_tombstones(&self) -> bool {
        self.0.source.has_tombstones()
    }
    fn held(&self) -> Vec<(String, String, String)> {
        let snap = self.0.source.snapshot();
        let alloc = self.0.alloc.lock();
        self.0
            .docs
            .lock()
            .keys()
            .filter_map(|n| {
                snap.get(n)
                    .map(|m| (n.clone(), m.uuid.clone(), alloc.get(n).cloned().unwrap_or_default()))
            })
            .collect()
    }
    fn note_allocation(&self, index: &str, allocation_id: &str) {
        self.0.alloc.lock().insert(index.to_string(), allocation_id.to_string());
    }
    fn drop_local(&self, index: &str) {
        self.0.docs.lock().remove(index);
        self.0.alloc.lock().remove(index);
    }
}

impl ShardHost for ModelSource {
    fn start_shard(
        &self,
        meta: &IndexMetadata,
        copy: &ShardRouting,
        _primary: Option<&NodeId>,
    ) -> Result<bool, String> {
        if copy.primary && copy.relocating_node.is_none() {
            // where the data is, or an empty primary someone accepted
            self.0.docs.lock().entry(meta.name.clone()).or_default();
            return Ok(true);
        }
        self.0.to_start.lock().push((meta.clone(), copy.clone()));
        Ok(false)
    }
    fn remove_shard(&self, index: &str, _shard: u32) {
        self.0.docs.lock().remove(index);
    }
}

/// One node of the simulated cluster: the coordinator and the model store.
pub struct ClusterNode {
    pub coord: Coordinator,
    me: NodeId,
    shared: Arc<Shared>,
    seq_no: BTreeMap<String, u64>,
    pending: BTreeMap<u64, PendingWrite>,
    /// a client's write forwarded to the primary: my rid -> (client, its rid)
    forwarded: BTreeMap<u64, (NodeId, u64)>,
    recoveries: BTreeMap<u64, Recovery>,
    next_rid: u64,
    pub timeout: Millis,
    pub accepted: Arc<parking_lot::Mutex<Vec<Accepted>>>,
    /// copies dropped or seen dropped, for the notes
    pub notes: bool,
}

impl ClusterNode {
    pub fn new(
        coord: Coordinator,
        source: Arc<MapSource>,
        accepted: Arc<parking_lot::Mutex<Vec<Accepted>>>,
    ) -> ClusterNode {
        let shared = Arc::new(Shared {
            docs: parking_lot::Mutex::new(Docs::new()),
            alloc: parking_lot::Mutex::new(BTreeMap::new()),
            to_start: parking_lot::Mutex::new(Vec::new()),
            source,
        });
        let mut coord = coord;
        let src = Arc::new(ModelSource(shared.clone()));
        coord.metadata = Some(src.clone());
        coord.host = Some(src);
        let me = coord.me.id.clone();
        ClusterNode {
            coord,
            me,
            shared,
            seq_no: BTreeMap::new(),
            pending: BTreeMap::new(),
            forwarded: BTreeMap::new(),
            recoveries: BTreeMap::new(),
            next_rid: 1 << 20,
            timeout: 5_000,
            accepted,
            notes: true,
        }
    }

    pub fn docs(&self) -> Docs {
        self.shared.docs.lock().clone()
    }

    fn rid(&mut self) -> u64 {
        self.next_rid += 1;
        self.next_rid
    }

    fn persist(&self, durable: &mut Durable) {
        let docs = self.shared.docs.lock();
        durable.entries.insert(D_DOCS.into(), serde_json::to_vec(&*docs).unwrap_or_default());
        let alloc = self.shared.alloc.lock();
        durable
            .entries
            .insert("model_alloc".into(), serde_json::to_vec(&*alloc).unwrap_or_default());
    }

    fn state(&self) -> &ClusterState {
        &self.coord.committed
    }

    fn primary_of(&self, index: &str) -> Option<NodeId> {
        self.state().routing.primary(index, 0).and_then(|p| p.node.clone())
    }

    fn holds(&self, index: &str) -> bool {
        self.state().routing.shards_of(index).any(|c| c.node.as_ref() == Some(&self.me))
    }

    fn is_data_action(action: &str) -> bool {
        matches!(action, CLIENT_WRITE | COPY_WRITE | SCAN)
    }

    // ---- the primary's side --------------------------------------------------

    fn accept(
        &mut self,
        client: NodeId,
        client_rid: u64,
        index: &str,
        id: &str,
        value: u64,
        clock: &dyn Clock,
        durable: &mut Durable,
    ) -> Vec<Output> {
        let term = self
            .state()
            .indices
            .get(index)
            .and_then(|m| m.primary_terms.get(&0).copied())
            .unwrap_or(1);
        let seq = {
            let e = self.seq_no.entry(index.to_string()).or_insert(0);
            let s = *e;
            *e += 1;
            s
        };
        let version = {
            let mut docs = self.shared.docs.lock();
            let d = docs.entry(index.to_string()).or_default();
            let version = d.get(id).map(|x| x.version + 1).unwrap_or(1);
            d.insert(id.to_string(), ModelDoc { version, seq, term, value });
            version
        };
        self.persist(durable);
        self.accepted.lock().push(Accepted {
            index: index.into(),
            term,
            seq,
            id: id.into(),
            value,
        });
        let targets = super::replication::targets(self.state(), &self.me, index, 0);
        let rid = self.rid();
        let mut out = Vec::new();
        let body = serde_json::to_vec(&json!({"index": index, "ops": [{"id": id, "version": version, "seq": seq, "term": term, "value": value}]})).unwrap_or_default();
        let mut waiting = BTreeSet::new();
        let mut in_sync_waiting = BTreeSet::new();
        for (node, in_sync) in targets {
            out.push(Output::Send {
                to: node.clone(),
                envelope: Envelope::request(COPY_WRITE, self.me.clone(), rid, body.clone()),
            });
            waiting.insert(node.clone());
            if in_sync {
                in_sync_waiting.insert(node);
            }
        }
        let mut pw = PendingWrite {
            client: client.clone(),
            client_rid,
            index: index.into(),
            id: id.into(),
            seq,
            term,
            waiting,
            in_sync_waiting,
            acked: false,
            deadline: clock.now() + self.timeout,
        };
        if pw.in_sync_waiting.is_empty() {
            pw.acked = true;
            out.push(Self::ack(&client, client_rid, &self.me, seq, term));
            let took: BTreeSet<NodeId> = BTreeSet::new();
            out.extend(self.stale_reports(index, &took));
        }
        self.pending.insert(rid, pw);
        out.push(Output::Timer { id: T_TICK, after: 500 });
        out
    }

    /// In-sync ids of this index that belong to no copy that took the write.
    fn stale_ids(&self, index: &str, took: &BTreeSet<NodeId>) -> Vec<(u32, String)> {
        let Some(m) = self.state().indices.get(index) else { return Vec::new() };
        let mut fine: Vec<String> = Vec::new();
        for c in self.state().routing.shards_of(index) {
            let held = c.node.as_ref() == Some(&self.me)
                || c.node.as_ref().map(|n| took.contains(n)).unwrap_or(false);
            if held && c.state != ShardState::Unassigned {
                if let Some(a) = &c.allocation_id {
                    fine.push(a.clone());
                }
            }
        }
        let mut out = Vec::new();
        for (shard, ids) in &m.in_sync_allocations {
            for id in ids {
                if !fine.contains(id) {
                    out.push((*shard, id.clone()));
                }
            }
        }
        out
    }

    fn stale_reports(&mut self, index: &str, took: &BTreeSet<NodeId>) -> Vec<Output> {
        let mut out = Vec::new();
        let Some(mgr) = self.state().cluster_manager.clone() else { return out };
        for (shard, id) in self.stale_ids(index, took) {
            let r = self.rid();
            let body = json!({"index": index, "shard": shard, "allocation_id": id});
            out.push(Output::Send {
                to: mgr.clone(),
                envelope: Envelope::request(
                    super::coordinator::SHARD_STALE,
                    self.me.clone(),
                    r,
                    serde_json::to_vec(&body).unwrap_or_default(),
                ),
            });
        }
        out
    }

    fn ack(client: &NodeId, client_rid: u64, me: &NodeId, seq: u64, term: u64) -> Output {
        let req = Envelope::request(CLIENT_WRITE, client.clone(), client_rid, vec![]);
        Output::Send {
            to: client.clone(),
            envelope: req.response(
                me.clone(),
                serde_json::to_vec(&json!({"acked": true, "seq": seq, "term": term}))
                    .unwrap_or_default(),
            ),
        }
    }

    fn refuse(client: &NodeId, client_rid: u64, me: &NodeId, why: &str) -> Output {
        let req = Envelope::request(CLIENT_WRITE, client.clone(), client_rid, vec![]);
        Output::Send { to: client.clone(), envelope: req.error(me.clone(), why) }
    }

    /// Copies that did not answer an acknowledged-by-in-sync write in time
    /// are reported to the manager, and the client is told no.
    fn tick(&mut self, clock: &dyn Clock) -> Vec<Output> {
        let now = clock.now();
        let mut out = Vec::new();
        let expired: Vec<u64> =
            self.pending.iter().filter(|(_, p)| now >= p.deadline).map(|(r, _)| *r).collect();
        for rid in expired {
            let p = self.pending.remove(&rid).unwrap();
            if !p.acked {
                out.push(Self::refuse(
                    &p.client,
                    p.client_rid,
                    &self.me,
                    "timeout waiting for the copies",
                ));
            }
            // the copies that never answered
            let manager = self.state().cluster_manager.clone();
            for node in p.waiting {
                if let Some(mgr) = &manager {
                    let copies: Vec<(u32, String)> = self
                        .state()
                        .routing
                        .shards_of(&p.index)
                        .filter(|c| c.node.as_ref() == Some(&node) && !c.primary)
                        .filter_map(|c| c.allocation_id.clone().map(|a| (c.shard, a)))
                        .collect();
                    for (shard, aid) in copies {
                        let r = self.rid();
                        let body = json!({"index": p.index, "shard": shard, "allocation_id": aid, "message": format!("no answer from {}", node.as_str())});
                        out.push(Output::Send {
                            to: mgr.clone(),
                            envelope: Envelope::request(
                                SHARD_FAILED,
                                self.me.clone(),
                                r,
                                serde_json::to_vec(&body).unwrap_or_default(),
                            ),
                        });
                    }
                }
            }
        }
        if !self.pending.is_empty() || !self.recoveries.is_empty() {
            out.push(Output::Timer { id: T_TICK, after: 500 });
        }
        out
    }

    // ---- recoveries --------------------------------------------------------------

    fn start_recoveries(&mut self) -> Vec<Output> {
        let asked: Vec<(IndexMetadata, ShardRouting)> =
            std::mem::take(&mut *self.shared.to_start.lock());
        let mut out = Vec::new();
        for (meta, copy) in asked {
            let Some(aid) = copy.allocation_id.clone() else { continue };
            let from = self.state().routing.primary(&meta.name, 0).and_then(|p| p.node.clone());
            let Some(from) = from.filter(|f| *f != self.me) else {
                // no primary to copy from: the copy is what it is
                out.extend(self.coord_input(Input::ShardDone {
                    allocation_id: aid,
                    result: Err("no primary to recover from".into()),
                }));
                continue;
            };
            // a copy is filled from nothing: what was here before is not the
            // primary's, and may hold writes the primary never took
            self.shared.docs.lock().insert(meta.name.clone(), BTreeMap::new());
            let rid = self.rid();
            let body =
                serde_json::to_vec(&json!({"index": meta.name, "from_seq": 0})).unwrap_or_default();
            out.push(Output::Send {
                to: from.clone(),
                envelope: Envelope::request(SCAN, self.me.clone(), rid, body),
            });
            self.recoveries.insert(
                rid,
                Recovery { index: meta.name.clone(), allocation_id: aid, from, next_seq: 0 },
            );
            out.push(Output::Timer { id: T_TICK, after: 500 });
        }
        out
    }

    /// Hand the coordinator an input of our own and keep its outputs.
    fn coord_input(&mut self, input: Input) -> Vec<Output> {
        // a durable the coordinator does not write for this input
        let mut scratch = Durable::default();
        let out = self.coord.handle(input, &NoClock, &mut scratch);
        out
    }

    fn apply_copy(&mut self, index: &str, ops: &[Value], durable: &mut Durable) -> usize {
        let mut n = 0;
        {
            let mut docs = self.shared.docs.lock();
            let d = docs.entry(index.to_string()).or_default();
            for op in ops {
                let id = op.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let version = op.get("version").and_then(|v| v.as_u64()).unwrap_or(1);
                let seq = op.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
                let term = op.get("term").and_then(|v| v.as_u64()).unwrap_or(1);
                let value = op.get("value").and_then(|v| v.as_u64()).unwrap_or(0);
                let newer = d.get(id).map(|x| version > x.version).unwrap_or(true);
                if newer {
                    d.insert(id.to_string(), ModelDoc { version, seq, term, value });
                    n += 1;
                }
                let e = self.seq_no.entry(index.to_string()).or_insert(0);
                *e = (*e).max(seq + 1);
            }
        }
        self.persist(durable);
        n
    }

    fn on_message(&mut self, e: Envelope, clock: &dyn Clock, durable: &mut Durable) -> Vec<Output> {
        let from = e.from.clone();
        match (e.action.as_str(), e.kind) {
            (CLIENT_WRITE, Kind::Request) => {
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let index = v.get("index").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let value = v.get("value").and_then(|x| x.as_u64()).unwrap_or(0);
                match self.primary_of(&index) {
                    Some(p) if p == self.me => {
                        self.accept(from, e.request_id, &index, &id, value, clock, durable)
                    }
                    Some(p) => {
                        // not here: carried to the primary, the answer relayed
                        let rid = self.rid();
                        self.forwarded.insert(rid, (from, e.request_id));
                        vec![Output::Send {
                            to: p,
                            envelope: Envelope::request(
                                CLIENT_WRITE,
                                self.me.clone(),
                                rid,
                                e.body.clone(),
                            ),
                        }]
                    }
                    None => vec![Self::refuse(&from, e.request_id, &self.me, "no primary")],
                }
            }
            (CLIENT_WRITE, Kind::Response) | (CLIENT_WRITE, Kind::Error) => {
                // an answer to a write this node forwarded
                let Some((client, client_rid)) = self.forwarded.remove(&e.request_id) else {
                    return vec![];
                };
                let req = Envelope::request(CLIENT_WRITE, client.clone(), client_rid, vec![]);
                let relayed = if e.kind == Kind::Response {
                    req.response(self.me.clone(), e.body.clone())
                } else {
                    req.error(self.me.clone(), &String::from_utf8_lossy(&e.body))
                };
                vec![Output::Send { to: client, envelope: relayed }]
            }
            (COPY_WRITE, Kind::Request) => {
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let index = v.get("index").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if !self.holds(&index) && !self.shared.docs.lock().contains_key(&index) {
                    return vec![Output::Send {
                        to: from,
                        envelope: e.error(self.me.clone(), "no copy here"),
                    }];
                }
                let ops: Vec<Value> =
                    v.get("ops").and_then(|o| o.as_array()).cloned().unwrap_or_default();
                // a primary of an older term is no primary: its writes are refused
                let known_term = self
                    .state()
                    .indices
                    .get(&index)
                    .and_then(|m| m.primary_terms.get(&0).copied())
                    .unwrap_or(1);
                if ops
                    .iter()
                    .any(|op| op.get("term").and_then(|t| t.as_u64()).unwrap_or(1) < known_term)
                {
                    return vec![Output::Send {
                        to: from,
                        envelope: e.error(self.me.clone(), "stale primary term"),
                    }];
                }
                self.apply_copy(&index, &ops, durable);
                vec![Output::Send { to: from, envelope: e.response(self.me.clone(), vec![]) }]
            }
            (COPY_WRITE, Kind::Response) => {
                let mut out = Vec::new();
                let mut acked_now: Option<(String, BTreeSet<NodeId>)> = None;
                // the copies of the index, read before the pending entry is held
                let holders: Vec<NodeId> = self
                    .pending
                    .get(&e.request_id)
                    .map(|p| {
                        self.state()
                            .routing
                            .shards_of(&p.index)
                            .filter_map(|c| c.node.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                let me = self.me.clone();
                if let Some(p) = self.pending.get_mut(&e.request_id) {
                    p.waiting.remove(&from);
                    p.in_sync_waiting.remove(&from);
                    if !p.acked && p.in_sync_waiting.is_empty() {
                        p.acked = true;
                        out.push(Self::ack(&p.client, p.client_rid, &me, p.seq, p.term));
                        let mut took: BTreeSet<NodeId> = BTreeSet::new();
                        took.insert(from.clone());
                        for n in &holders {
                            if !p.waiting.contains(n) {
                                took.insert(n.clone());
                            }
                        }
                        acked_now = Some((p.index.clone(), took));
                    }
                    if p.waiting.is_empty() {
                        self.pending.remove(&e.request_id);
                    }
                }
                if let Some((index, took)) = acked_now {
                    out.extend(self.stale_reports(&index, &took));
                }
                out
            }
            (COPY_WRITE, Kind::Error) => {
                // the copy refused: it is failed, and the write is not acknowledged by it
                let mut out = Vec::new();
                if let Some(mut p) = self.pending.remove(&e.request_id) {
                    p.waiting.remove(&from);
                    if !p.acked {
                        out.push(Self::refuse(
                            &p.client,
                            p.client_rid,
                            &self.me,
                            "a copy refused the write",
                        ));
                    }
                    if let Some(mgr) = self.state().cluster_manager.clone() {
                        let copies: Vec<(u32, String)> = self
                            .state()
                            .routing
                            .shards_of(&p.index)
                            .filter(|c| c.node.as_ref() == Some(&from) && !c.primary)
                            .filter_map(|c| c.allocation_id.clone().map(|a| (c.shard, a)))
                            .collect();
                        for (shard, aid) in copies {
                            let r = self.rid();
                            let body = json!({"index": p.index, "shard": shard, "allocation_id": aid, "message": "refused a write"});
                            out.push(Output::Send {
                                to: mgr.clone(),
                                envelope: Envelope::request(
                                    SHARD_FAILED,
                                    self.me.clone(),
                                    r,
                                    serde_json::to_vec(&body).unwrap_or_default(),
                                ),
                            });
                        }
                    }
                }
                out
            }
            (SCAN, Kind::Request) => {
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let index = v.get("index").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let from_seq = v.get("from_seq").and_then(|x| x.as_u64()).unwrap_or(0);
                // the primary's copy, empty if nothing was written yet
                let am_primary = self.primary_of(&index).as_ref() == Some(&self.me);
                let mut docs = self.shared.docs.lock();
                if am_primary {
                    docs.entry(index.clone()).or_default();
                }
                let Some(d) = docs.get(&index) else {
                    drop(docs);
                    return vec![Output::Send {
                        to: from,
                        envelope: e.error(self.me.clone(), "no index here"),
                    }];
                };
                let mut all: Vec<(&String, &ModelDoc)> =
                    d.iter().filter(|(_, x)| x.seq >= from_seq).collect();
                all.sort_by_key(|(_, x)| x.seq);
                let page: Vec<Value> = all
                    .iter()
                    .take(50)
                    .map(|(id, x)| json!({"id": id, "version": x.version, "seq": x.seq, "term": x.term, "value": x.value}))
                    .collect();
                let next = if all.len() > 50 { Some(all[49].1.seq + 1) } else { None };
                drop(docs);
                let body =
                    serde_json::to_vec(&json!({"ops": page, "next_seq": next})).unwrap_or_default();
                vec![Output::Send { to: from, envelope: e.response(self.me.clone(), body) }]
            }
            (SCAN, Kind::Response) => {
                let Some(r) = self.recoveries.remove(&e.request_id) else { return vec![] };
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let ops: Vec<Value> =
                    v.get("ops").and_then(|o| o.as_array()).cloned().unwrap_or_default();
                self.apply_copy(&r.index, &ops, durable);
                match v.get("next_seq").and_then(|n| n.as_u64()) {
                    Some(next) if next > r.next_seq => {
                        let rid = self.rid();
                        let body = serde_json::to_vec(&json!({"index": r.index, "from_seq": next}))
                            .unwrap_or_default();
                        let to = r.from.clone();
                        self.recoveries.insert(rid, Recovery { next_seq: next, ..r });
                        vec![Output::Send {
                            to,
                            envelope: Envelope::request(SCAN, self.me.clone(), rid, body),
                        }]
                    }
                    _ => self.coord_input(Input::ShardDone {
                        allocation_id: r.allocation_id,
                        result: Ok(()),
                    }),
                }
            }
            (SCAN, Kind::Error) => {
                let Some(r) = self.recoveries.remove(&e.request_id) else { return vec![] };
                self.coord_input(Input::ShardDone {
                    allocation_id: r.allocation_id,
                    result: Err(format!(
                        "recovery from {} failed: {}",
                        r.from.as_str(),
                        String::from_utf8_lossy(&e.body)
                    )),
                })
            }
            _ => vec![],
        }
    }
}

struct NoClock;
impl Clock for NoClock {
    fn now(&self) -> Millis {
        0
    }
    fn wall(&self) -> Millis {
        0
    }
}

impl NodeLogic for ClusterNode {
    fn handle(&mut self, input: Input, clock: &dyn Clock, durable: &mut Durable) -> Vec<Output> {
        let mut out = match input {
            Input::Start => {
                if let Some(bytes) = durable.entries.get("model_alloc") {
                    if let Ok(a) = serde_json::from_slice::<BTreeMap<String, String>>(bytes) {
                        *self.shared.alloc.lock() = a;
                    }
                }
                if let Some(bytes) = durable.entries.get(D_DOCS) {
                    if let Ok(d) = serde_json::from_slice::<Docs>(bytes) {
                        *self.shared.docs.lock() = d;
                        for (index, docs) in self.shared.docs.lock().iter() {
                            let max = docs.values().map(|x| x.seq + 1).max().unwrap_or(0);
                            self.seq_no.insert(index.clone(), max);
                        }
                    }
                }
                let mut out = self.coord.handle(Input::Start, clock, durable);
                out.push(Output::Timer { id: T_TICK, after: 500 });
                out
            }
            Input::Message(e) if Self::is_data_action(&e.action) => {
                self.on_message(e, clock, durable)
            }
            Input::Timer(T_TICK) => self.tick(clock),
            other => self.coord.handle(other, clock, durable),
        };
        // copies the coordinator asked for since: started from the primary
        out.extend(self.start_recoveries());
        self.persist(durable);
        out
    }
}

/// A client of the simulated cluster: writes documents with unique ids to
/// whichever node, one every `interval`, and remembers what was
/// acknowledged.
pub struct Client {
    me: NodeId,
    nodes: Vec<NodeId>,
    index: String,
    count: u64,
    pub sent: u64,
    interval: Millis,
    /// how long after starting the first write goes: the cluster needs a
    /// moment to place the index
    pub start_delay: Millis,
    pub pending: BTreeMap<u64, (String, u64, Millis)>,
    rng: super::sim::Rng,
    pub acked: Arc<parking_lot::Mutex<Vec<(String, String, u64, u64, u64)>>>,
    pub refused: Arc<parking_lot::Mutex<u64>>,
}

const T_CLIENT: u64 = 200;

impl Client {
    pub fn new(
        me: NodeId,
        nodes: Vec<NodeId>,
        index: &str,
        count: u64,
        interval: Millis,
        seed: u64,
    ) -> Client {
        Client {
            me,
            nodes,
            index: index.into(),
            count,
            sent: 0,
            interval,
            start_delay: 3_000,
            pending: BTreeMap::new(),
            rng: super::sim::Rng::new(seed),
            acked: Arc::default(),
            refused: Arc::default(),
        }
    }
}

impl NodeLogic for Client {
    fn handle(&mut self, input: Input, clock: &dyn Clock, _durable: &mut Durable) -> Vec<Output> {
        match input {
            Input::Start => vec![Output::Timer { id: T_CLIENT, after: self.start_delay }],
            Input::Timer(T_CLIENT) => {
                let mut out = Vec::new();
                // writes that got no answer at all count as refused
                let now = clock.now();
                let stale: Vec<u64> = self
                    .pending
                    .iter()
                    .filter(|(_, (_, _, at))| now - at > 20_000)
                    .map(|(r, _)| *r)
                    .collect();
                for r in stale {
                    self.pending.remove(&r);
                    *self.refused.lock() += 1;
                }
                if self.sent < self.count {
                    let id = format!("d{}", self.sent);
                    let value = self.sent * 7 + 1;
                    self.sent += 1;
                    let to =
                        self.nodes[self.rng.range(0, self.nodes.len() as u64 - 1) as usize].clone();
                    let rid = 1_000_000 + self.sent;
                    self.pending.insert(rid, (id.clone(), value, now));
                    let body =
                        serde_json::to_vec(&json!({"index": self.index, "id": id, "value": value}))
                            .unwrap_or_default();
                    out.push(Output::Note(format!("write {id} -> {to}")));
                    out.push(Output::Send {
                        to,
                        envelope: Envelope::request(CLIENT_WRITE, self.me.clone(), rid, body),
                    });
                }
                if self.sent < self.count || !self.pending.is_empty() {
                    out.push(Output::Timer { id: T_CLIENT, after: self.interval });
                }
                out
            }
            Input::Message(e) if e.action == CLIENT_WRITE => {
                let Some((id, value, _)) = self.pending.remove(&e.request_id) else {
                    return vec![];
                };
                let note = Output::Note(format!(
                    "answer for {id}: {:?} {}",
                    e.kind,
                    String::from_utf8_lossy(&e.body)
                ));
                let _ = &note;
                if e.kind == Kind::Response {
                    let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                    let seq = v.get("seq").and_then(|x| x.as_u64()).unwrap_or(0);
                    let term = v.get("term").and_then(|x| x.as_u64()).unwrap_or(0);
                    self.acked.lock().push((self.index.clone(), id, value, seq, term));
                } else {
                    *self.refused.lock() += 1;
                }
                vec![note]
            }
            _ => vec![],
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::cluster::sim::Sim;
    use crate::cluster::state::DiscoveryNode;

    fn node(name: &str) -> DiscoveryNode {
        DiscoveryNode {
            id: NodeId(name.into()),
            name: name.into(),
            ephemeral_id: NodeId::random(),
            transport_address: format!("127.0.0.1:{name}"),
            roles: vec!["cluster_manager".into(), "data".into()],
            attributes: BTreeMap::new(),
        }
    }

    fn index_meta(name: &str, replicas: u32) -> IndexMetadata {
        IndexMetadata {
            name: name.into(),
            uuid: format!("{name}-uuid"),
            version: 1,
            mapping_version: 1,
            settings_version: 1,
            aliases_version: 1,
            state: "open".into(),
            settings: json!({"index": {"number_of_shards": "1", "number_of_replicas": replicas.to_string(), "unassigned": {"node_left": {"delayed_timeout": "1s"}}}}),
            mappings: json!({}),
            aliases: json!({}),
            number_of_shards: 1,
            number_of_replicas: replicas,
            primary_terms: BTreeMap::new(),
            in_sync_allocations: BTreeMap::new(),
            creation_date: 0,
        }
    }

    /// The cluster under test, with the handles the checks read.
    pub struct Lab {
        pub seed: u64,
        pub sim: Sim,
        pub nodes: Vec<NodeId>,
        pub client: NodeId,
        pub accepted: Arc<parking_lot::Mutex<Vec<Accepted>>>,
        pub acked: Arc<parking_lot::Mutex<Vec<(String, String, u64, u64, u64)>>>,
        pub refused: Arc<parking_lot::Mutex<u64>>,
        pub docs_of: BTreeMap<NodeId, Arc<parking_lot::Mutex<Docs>>>,
    }

    pub fn lab(
        seed: u64,
        names: &[&'static str],
        index: &str,
        replicas: u32,
        writes: u64,
        interval: Millis,
    ) -> Lab {
        let mut sim = Sim::new(seed);
        let ids: Vec<NodeId> = names.iter().map(|n| NodeId((*n).into())).collect();
        let mut map = BTreeMap::new();
        map.insert(index.to_string(), index_meta(index, replicas));
        let source = Arc::new(MapSource::new(map));
        let accepted: Arc<parking_lot::Mutex<Vec<Accepted>>> = Arc::default();
        let mut docs_of = BTreeMap::new();
        for n in names {
            let n: &'static str = n;
            let seeds: Vec<NodeId> = ids.iter().filter(|i| i.as_str() != n).cloned().collect();
            let initial: Vec<String> = names.iter().map(|s| s.to_string()).collect();
            let src = source.clone();
            let acc = accepted.clone();
            // the docs live in a map the checks can read after a crash too:
            // the node's factory is handed the same Arc each time it is made
            let docs_handle: Arc<parking_lot::Mutex<Docs>> = Arc::default();
            docs_of.insert(NodeId(n.into()), docs_handle.clone());
            sim.add_node(
                NodeId(n.into()),
                Box::new(move |_| {
                    let c = Coordinator::new(node(n), "c", "u", initial.clone(), seeds.clone());
                    let mut cn = ClusterNode::new(c, src.clone(), acc.clone());
                    cn.coord.notes = true;
                    // the shared docs map the checks read is the node's own
                    let mine = cn.shared.clone();
                    let handle = docs_handle.clone();
                    // keep the handle pointing at this incarnation's docs
                    *handle.lock() = Docs::new();
                    let _ = mine;
                    Box::new(Mirror { inner: cn, mirror: handle })
                }),
            );
        }
        let client = NodeId("client".into());
        let acked: Arc<parking_lot::Mutex<Vec<(String, String, u64, u64, u64)>>> = Arc::default();
        let refused: Arc<parking_lot::Mutex<u64>> = Arc::default();
        let cnodes = ids.clone();
        let index_s = index.to_string();
        let (acked2, refused2) = (acked.clone(), refused.clone());
        sim.add_node(
            client.clone(),
            Box::new(move |_| {
                let mut c = Client::new(
                    NodeId("client".into()),
                    cnodes.clone(),
                    &index_s,
                    writes,
                    interval,
                    seed,
                );
                c.acked = acked2.clone();
                c.refused = refused2.clone();
                Box::new(c)
            }),
        );
        Lab { seed, sim, nodes: ids, client, accepted, acked, refused, docs_of }
    }

    /// A node that mirrors its docs into a handle the checks hold.
    struct Mirror {
        inner: ClusterNode,
        mirror: Arc<parking_lot::Mutex<Docs>>,
    }

    impl NodeLogic for Mirror {
        fn handle(
            &mut self,
            input: Input,
            clock: &dyn Clock,
            durable: &mut Durable,
        ) -> Vec<Output> {
            let out = self.inner.handle(input, clock, durable);
            *self.mirror.lock() = self.inner.docs();
            out
        }
    }

    fn committed(sim: &Sim, id: &NodeId) -> Option<ClusterState> {
        let bytes = sim.durable(id)?.entries.get(super::super::coordinator::D_COMMITTED)?;
        serde_json::from_slice(bytes).ok()
    }

    /// The docs a node holds: what it mirrored while up, else what its disk has.
    fn docs_now(lab: &Lab, id: &NodeId) -> Docs {
        if lab.sim.is_up(id) {
            return lab.docs_of[id].lock().clone();
        }
        lab.sim
            .durable(id)
            .and_then(|d| d.entries.get(D_DOCS))
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or_default()
    }

    /// Invariant one: every acknowledged write is on every active copy of
    /// its index, and on the primary, with the value that was written.
    pub fn nothing_acknowledged_is_lost(lab: &Lab, index: &str) {
        let acked = lab.acked.lock().clone();
        let up: Vec<NodeId> = lab.nodes.iter().filter(|n| lab.sim.is_up(n)).cloned().collect();
        let state = committed(&lab.sim, &up[0]).expect("a committed state");
        let holders: Vec<NodeId> = state
            .routing
            .shards_of(index)
            .filter(|c| matches!(c.state, ShardState::Started | ShardState::Relocating))
            .filter_map(|c| c.node.clone())
            .filter(|n| lab.sim.is_up(n))
            .collect();
        assert!(!holders.is_empty(), "no active copy of {index} at the end");
        for holder in &holders {
            let docs = docs_now(lab, holder);
            let d = docs.get(index).cloned().unwrap_or_default();
            for (_, id, value, _, _) in &acked {
                match d.get(id) {
                    Some(x) => assert_eq!(
                        x.value, *value,
                        "acknowledged write {id} on {holder} has another value"
                    ),
                    None => panic!(
                        "acknowledged write {id} is missing on {holder} ({} docs there, {} acked)",
                        d.len(),
                        acked.len()
                    ),
                }
            }
        }
    }

    /// Invariant two: no two nodes accepted a write as the primary of one
    /// index in one term with the same sequence number.
    pub fn no_two_primaries_accepted_writes(lab: &Lab) {
        let acc = lab.accepted.lock().clone();
        let mut seen: BTreeMap<(String, u64, u64), (String, u64)> = BTreeMap::new();
        for a in &acc {
            let key = (a.index.clone(), a.term, a.seq);
            if let Some((id, value)) = seen.get(&key) {
                assert!(
                    *id == a.id && *value == a.value,
                    "two primaries accepted different writes for {key:?}: {id}={value} and {}={}",
                    a.id,
                    a.value
                );
            } else {
                seen.insert(key, (a.id.clone(), a.value));
            }
        }
    }

    /// Invariant three: every active copy of the index holds the same documents.
    pub fn no_divergence(lab: &Lab, index: &str) {
        let up: Vec<NodeId> = lab.nodes.iter().filter(|n| lab.sim.is_up(n)).cloned().collect();
        let state = committed(&lab.sim, &up[0]).expect("a committed state");
        let holders: Vec<NodeId> = state
            .routing
            .shards_of(index)
            .filter(|c| matches!(c.state, ShardState::Started | ShardState::Relocating))
            .filter_map(|c| c.node.clone())
            .filter(|n| lab.sim.is_up(n))
            .collect();
        let sets: Vec<BTreeMap<String, u64>> = holders
            .iter()
            .map(|h| {
                docs_now(lab, h)
                    .get(index)
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.value))
                    .collect()
            })
            .collect();
        for (i, s) in sets.iter().enumerate().skip(1) {
            let only_first: Vec<&String> = sets[0].keys().filter(|k| !s.contains_key(*k)).collect();
            let only_this: Vec<&String> = s.keys().filter(|k| !sets[0].contains_key(*k)).collect();
            assert_eq!(
                &sets[0],
                s,
                "seed {}: copies on {} and {} differ ({} vs {} docs); only on {}: {:?}; only on {}: {:?}; state {:?}",
                lab.seed,
                holders[0],
                holders[i],
                sets[0].len(),
                s.len(),
                holders[0],
                only_first,
                holders[i],
                only_this,
                state
                    .routing
                    .shards_of(index)
                    .map(|c| format!("{:?}/{}@{:?}", c.state, c.primary, c.node))
                    .collect::<Vec<_>>()
            );
        }
    }

    fn quiet(lab: &mut Lab, until: Millis) {
        lab.sim.run_until(until);
    }

    /// A node's copy keeps the allocation id the manager gave it across a
    /// restart, and reports it as held.
    #[test]
    fn a_copy_remembers_its_allocation_id_across_a_restart() {
        let mut map = BTreeMap::new();
        map.insert("solo".to_string(), index_meta("solo", 0));
        let source = Arc::new(MapSource::new(map));
        let c = Coordinator::new(node("a"), "c", "u", vec!["a".into()], vec![]);
        let mut cn = ClusterNode::new(c, source.clone(), Arc::default());
        let src = cn.coord.metadata.clone().unwrap();
        cn.shared.docs.lock().entry("solo".into()).or_default();
        src.note_allocation("solo", "aid-1");
        let mut durable = Durable::default();
        cn.persist(&mut durable);
        assert_eq!(
            src.held(),
            vec![("solo".to_string(), "solo-uuid".to_string(), "aid-1".to_string())]
        );
        // a fresh node from the same disk
        let c2 = Coordinator::new(node("a"), "c", "u", vec!["a".into()], vec![]);
        let mut cn2 = ClusterNode::new(c2, source, Arc::default());
        let _ = cn2.handle(Input::Start, &NoClock, &mut durable);
        let src2 = cn2.coord.metadata.clone().unwrap();
        assert_eq!(
            src2.held(),
            vec![("solo".to_string(), "solo-uuid".to_string(), "aid-1".to_string())]
        );
    }

    #[test]
    fn writes_reach_every_copy_and_are_acknowledged() {
        let mut lab = lab(1, &["a", "b", "c"], "i", 1, 60, 50);
        quiet(&mut lab, 40_000);
        let acked = lab.acked.lock().len();
        assert_eq!(acked, 60, "only {acked} acknowledged, {} refused", lab.refused.lock());
        nothing_acknowledged_is_lost(&lab, "i");
        no_two_primaries_accepted_writes(&lab);
        no_divergence(&lab, "i");
    }

    #[test]
    fn the_primary_crashes_mid_stream_and_nothing_acknowledged_is_lost() {
        let mut lab = lab(2, &["a", "b", "c"], "i", 1, 200, 50);
        quiet(&mut lab, 8_000);
        let state = committed(&lab.sim, &lab.nodes[0]).unwrap();
        let primary = state.routing.primary("i", 0).unwrap().node.clone().unwrap();
        lab.sim.crash(&primary);
        quiet(&mut lab, 30_000);
        lab.sim.restart(&primary);
        quiet(&mut lab, 80_000);
        let acked = lab.acked.lock().len();
        assert!(acked >= 100, "only {acked} acknowledged");
        nothing_acknowledged_is_lost(&lab, "i");
        no_two_primaries_accepted_writes(&lab);
        no_divergence(&lab, "i");
        // the promoted copy is in a new term
        let state = committed(&lab.sim, &lab.nodes[0]).unwrap();
        assert!(state.indices["i"].primary_terms[&0] >= 2);
    }

    #[test]
    fn a_lone_primary_that_crashes_comes_back_with_its_data() {
        let mut lab = lab(3, &["a", "b", "c"], "solo", 0, 80, 50);
        quiet(&mut lab, 10_000);
        let state = committed(&lab.sim, &lab.nodes[0]).unwrap();
        let primary = state.routing.primary("solo", 0).unwrap().node.clone().unwrap();
        let acked_before = lab.acked.lock().len();
        assert!(acked_before > 20);
        lab.sim.crash(&primary);
        quiet(&mut lab, 25_000);
        // red while the data is away
        let up: Vec<NodeId> = lab.nodes.iter().filter(|n| lab.sim.is_up(n)).cloned().collect();
        let s = committed(&lab.sim, &up[0]).unwrap();
        assert_eq!(
            s.health_status(None),
            "red",
            "{:?}",
            s.routing.shards_of("solo").collect::<Vec<_>>()
        );
        lab.sim.restart(&primary);
        quiet(&mut lab, 70_000);
        let s = committed(&lab.sim, &up[0]).unwrap();
        assert_eq!(
            s.health_status(None),
            "green",
            "{:?}",
            s.routing.shards_of("solo").collect::<Vec<_>>()
        );
        assert_eq!(s.routing.primary("solo", 0).unwrap().node, Some(primary));
        nothing_acknowledged_is_lost(&lab, "solo");
        no_two_primaries_accepted_writes(&lab);
    }

    #[test]
    fn the_primary_is_cut_off_and_no_acknowledged_write_is_lost() {
        let mut lab = lab(4, &["a", "b", "c"], "i", 1, 200, 50);
        quiet(&mut lab, 8_000);
        let state = committed(&lab.sim, &lab.nodes[0]).unwrap();
        let primary = state.routing.primary("i", 0).unwrap().node.clone().unwrap();
        let others: Vec<NodeId> = lab.nodes.iter().filter(|n| **n != primary).cloned().collect();
        // the client can still reach everyone; the primary cannot reach the others
        lab.sim.partition(&[primary.clone()], &others);
        quiet(&mut lab, 40_000);
        lab.sim.heal();
        quiet(&mut lab, 100_000);
        nothing_acknowledged_is_lost(&lab, "i");
        no_two_primaries_accepted_writes(&lab);
        no_divergence(&lab, "i");
    }

    /// The storm one seed makes: crashes, restarts and partitions, then
    /// everything back and a long quiet.
    fn storm(seed: u64) -> (Lab, Vec<String>) {
        let mut lab = lab(seed, &["a", "b", "c"], "i", 1, 150, 40);
        let mut rng = super::super::sim::Rng::new(seed);
        let mut t = 4_000;
        let mut events = Vec::new();
        for _ in 0..5 {
            quiet(&mut lab, t);
            let victim = lab.nodes[rng.range(0, 2) as usize].clone();
            match rng.range(0, 2) {
                0 => {
                    if lab.sim.is_up(&victim) {
                        events.push(format!("{t} crash {victim}"));
                        lab.sim.crash(&victim);
                    } else {
                        events.push(format!("{t} restart {victim}"));
                        lab.sim.restart(&victim);
                    }
                }
                1 => {
                    let others: Vec<NodeId> =
                        lab.nodes.iter().filter(|n| **n != victim).cloned().collect();
                    lab.sim.partition(&[victim.clone()], &others);
                    let heal_at = t + 3_000 + rng.range(0, 4_000);
                    events.push(format!("{t} isolate {victim} until {heal_at}"));
                    lab.sim.heal_at(heal_at);
                }
                _ => {}
            }
            t += 3_000 + rng.range(0, 4_000);
        }
        for n in lab.nodes.clone() {
            if !lab.sim.is_up(&n) {
                events.push(format!("{t} restart {n}"));
                lab.sim.restart(&n);
            }
        }
        lab.sim.heal();
        events.push(format!("{t} heal"));
        quiet(&mut lab, t + 120_000);
        (lab, events)
    }

    #[test]
    fn crashes_restarts_and_partitions_across_seeds_keep_the_invariants() {
        for seed in 20..32 {
            let (lab, _) = storm(seed);
            let acked = lab.acked.lock().len();
            assert!(acked > 0, "seed {seed}: nothing acknowledged");
            nothing_acknowledged_is_lost(&lab, "i");
            no_two_primaries_accepted_writes(&lab);
            no_divergence(&lab, "i");
        }
    }

    /// `MODEL_SEEDS=a..b cargo test many_seeds -- --nocapture`: the storm over
    /// a range of seeds, for a longer look than the suite takes.
    #[test]
    fn many_seeds() {
        let Ok(range) = std::env::var("MODEL_SEEDS") else { return };
        let (a, b) = range
            .split_once("..")
            .map(|(a, b)| (a.parse::<u64>().unwrap_or(0), b.parse::<u64>().unwrap_or(0)))
            .unwrap_or((0, 0));
        let mut broken = Vec::new();
        for seed in a..b {
            let r = std::panic::catch_unwind(|| {
                let (lab, _) = storm(seed);
                nothing_acknowledged_is_lost(&lab, "i");
                no_two_primaries_accepted_writes(&lab);
                no_divergence(&lab, "i");
                lab.acked.lock().len()
            });
            match r {
                Ok(n) => eprintln!("seed {seed}: ok, {n} acknowledged"),
                Err(_) => {
                    eprintln!("seed {seed}: BROKEN");
                    broken.push(seed);
                }
            }
        }
        assert!(broken.is_empty(), "broken seeds: {broken:?}");
    }

    /// `MODEL_SEED=n cargo test replay_one_seed -- --nocapture`: the storm of
    /// one seed with its events and the cluster's notes, for reading.
    #[test]
    fn replay_one_seed() {
        let Ok(seed) = std::env::var("MODEL_SEED").map(|s| s.parse::<u64>().unwrap_or(20)) else {
            return;
        };
        let (lab, events) = storm(seed);
        for e in &events {
            eprintln!("EVENT {e}");
        }
        for (at, n, t) in lab.sim.notes.iter() {
            if !t.starts_with("committed") && !t.starts_with("write ") && !t.starts_with("answer ")
            {
                eprintln!("{at} {n} {t}");
            }
        }
        eprintln!("acked {} refused {}", lab.acked.lock().len(), lab.refused.lock());
        no_divergence(&lab, "i");
    }
}
