//! Joining, leaving, and telling everyone: the cluster manager's work,
//! with the manager fixed for now (an election chooses it in 6.4).
//!
//! A node that is not the manager sends a join to the manager it was
//! told about (`cluster.initial_cluster_manager_nodes`, or the seed hosts
//! that answered); the manager adds it to the state and publishes the
//! new state in two phases -- every node accepts it, then is told to
//! commit it -- so that no node applies a state the others may never
//! see. The manager checks its followers on a timer and drops one that
//! stops answering; a follower that stops hearing from its manager goes
//! back to looking for one. All of it is `NodeLogic`, so it runs the
//! same under the simulation and under the production runtime.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::clock::{Clock, Millis};
use super::sim::{Durable, Input, NodeLogic, Output};
use super::state::{ClusterState, DiscoveryNode};
use super::transport::{Envelope, Kind, NodeId};

pub const JOIN: &str = "internal:cluster/coordination/join";
pub const PUBLISH: &str = "internal:cluster/coordination/publish_state";
pub const COMMIT: &str = "internal:cluster/coordination/commit_state";
pub const FOLLOWER_CHECK: &str = "internal:coordination/fault_detection/follower_check";
pub const LEADER_CHECK: &str = "internal:coordination/fault_detection/leader_check";
pub const LEAVE: &str = "internal:cluster/coordination/leave";

/// Timers, by id.
const T_JOIN: u64 = 1;
const T_FOLLOWER_CHECK: u64 = 2;
const T_LEADER_CHECK: u64 = 3;
const T_METADATA: u64 = 4;

/// The timings, in milliseconds; the plugin's defaults where it has them.
#[derive(Clone, Debug)]
pub struct Timings {
    pub join_retry: Millis,
    pub follower_check_interval: Millis,
    pub follower_check_timeout: Millis,
    pub follower_check_retries: u32,
    pub leader_check_interval: Millis,
    pub leader_check_timeout: Millis,
    pub leader_check_retries: u32,
}

impl Default for Timings {
    fn default() -> Timings {
        Timings {
            join_retry: 1_000,
            follower_check_interval: 1_000,
            follower_check_timeout: 10_000,
            follower_check_retries: 3,
            leader_check_interval: 1_000,
            leader_check_timeout: 10_000,
            leader_check_retries: 3,
        }
    }
}

/// What a node believes about the cluster right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// looking for a manager to join
    Candidate,
    /// following the named manager
    Follower(NodeId),
    /// the manager
    Leader,
}

pub struct Coordinator {
    pub me: DiscoveryNode,
    pub timings: Timings,
    /// the node the settings name as the manager (fixed until 6.4)
    pub configured_manager: Option<NodeId>,
    /// nodes to ask when looking for the manager
    pub seeds: Vec<NodeId>,
    pub mode: Mode,
    /// the state this node has committed
    pub committed: ClusterState,
    /// a state accepted but not yet committed
    pub accepted: Option<ClusterState>,
    /// the manager's own bookkeeping
    followers: BTreeMap<NodeId, FollowerHealth>,
    /// acknowledgements of the state being published, by version
    publishing: Option<Publication>,
    next_request: u64,
    /// unanswered follower checks, by request id
    pending_checks: BTreeMap<u64, NodeId>,
    leader_misses: u32,
    leader_check_outstanding: Option<u64>,
    /// nodes that asked to join while a publication was in flight
    waiting_joins: Vec<DiscoveryNode>,
    pub notes: bool,
    /// where the manager reads index metadata, and how often it looks
    pub metadata: Option<std::sync::Arc<dyn super::metadata::MetadataSource>>,
    pub metadata_poll: Millis,
    last_fingerprint: u64,
    allocations: super::metadata::Allocations,
    /// a publication asked for while one was in flight
    republish_wanted: bool,
}

#[derive(Clone, Debug, Default)]
struct FollowerHealth {
    misses: u32,
}

#[derive(Clone, Debug)]
struct Publication {
    state: ClusterState,
    acked: BTreeSet<NodeId>,
    committed_sent: bool,
}

impl Coordinator {
    pub fn new(
        me: DiscoveryNode,
        cluster_name: &str,
        cluster_uuid: &str,
        configured_manager: Option<NodeId>,
        seeds: Vec<NodeId>,
    ) -> Coordinator {
        Coordinator {
            me,
            timings: Timings::default(),
            configured_manager,
            seeds,
            mode: Mode::Candidate,
            committed: ClusterState::empty(cluster_name, cluster_uuid),
            accepted: None,
            followers: BTreeMap::new(),
            publishing: None,
            next_request: 1,
            pending_checks: BTreeMap::new(),
            leader_misses: 0,
            leader_check_outstanding: None,
            waiting_joins: Vec::new(),
            notes: true,
            metadata: None,
            metadata_poll: 500,
            last_fingerprint: 0,
            allocations: super::metadata::Allocations::default(),
            republish_wanted: false,
        }
    }

    /// Lay the manager's index metadata and shard placement over a state.
    fn place(&mut self, s: &mut ClusterState, clock: &dyn Clock) {
        let Some(src) = self.metadata.clone() else { return };
        let snapshot = src.snapshot();
        let manager = self.me.id.clone();
        let names: Vec<String> = s.indices.keys().cloned().collect();
        for gone in names.iter().filter(|n| !snapshot.contains_key(*n)) {
            self.allocations.forget_index(gone);
        }
        let routing = super::metadata::build_routing(
            &snapshot,
            &manager,
            &mut self.allocations,
            clock.wall(),
        );
        s.indices = snapshot
            .into_iter()
            .map(|(n, m)| (n, super::metadata::with_terms(m, &routing)))
            .collect();
        s.routing = routing;
        self.last_fingerprint = super::metadata::fingerprint(&s.indices);
    }

    fn rid(&mut self) -> u64 {
        self.next_request += 1;
        self.next_request
    }

    fn note(&self, text: String) -> Output {
        Output::Note(text)
    }

    fn i_am_manager(&self) -> bool {
        self.configured_manager.as_ref() == Some(&self.me.id)
    }

    /// The committed state, as `_cluster/state` reads it.
    pub fn state(&self) -> &ClusterState {
        &self.committed
    }

    // ---- becoming the manager ------------------------------------------------------

    fn become_leader(&mut self, clock: &dyn Clock, durable: &mut Durable) -> Vec<Output> {
        self.mode = Mode::Leader;
        let mut s = self.committed.next();
        s.term = s.term.max(1);
        s.cluster_manager = Some(self.me.id.clone());
        s.cluster_uuid_committed = true;
        s.nodes.insert(self.me.id.clone(), self.me.clone());
        if !s.last_committed_config.contains(&self.me.id) {
            s.last_committed_config.push(self.me.id.clone());
            s.last_accepted_config.push(self.me.id.clone());
        }
        self.place(&mut s, clock);
        let mut out = self.publish(s, clock, durable);
        out.push(Output::Timer {
            id: T_FOLLOWER_CHECK,
            after: self.timings.follower_check_interval,
        });
        if self.metadata.is_some() {
            out.push(Output::Timer { id: T_METADATA, after: self.metadata_poll });
        }
        out
    }

    /// Publish a state: send it to every node in it, commit when all
    /// have accepted (or when the only node is this one).
    fn publish(
        &mut self,
        state: ClusterState,
        _clock: &dyn Clock,
        durable: &mut Durable,
    ) -> Vec<Output> {
        let mut out = Vec::new();
        let others: Vec<NodeId> =
            state.nodes.keys().filter(|n| **n != self.me.id).cloned().collect();
        let body = serde_json::to_vec(&state).unwrap_or_default();
        let rid = self.rid();
        for n in &others {
            out.push(Output::Send {
                to: n.clone(),
                envelope: Envelope::request(PUBLISH, self.me.id.clone(), rid, body.clone()),
            });
        }
        let mut acked = BTreeSet::new();
        acked.insert(self.me.id.clone());
        self.publishing = Some(Publication { state: state.clone(), acked, committed_sent: false });
        self.accepted = Some(state);
        if others.is_empty() {
            out.extend(self.commit_publication(durable));
        }
        out
    }

    fn commit_publication(&mut self, durable: &mut Durable) -> Vec<Output> {
        let mut out = Vec::new();
        let Some(p) = self.publishing.as_mut() else { return out };
        if p.committed_sent {
            return out;
        }
        p.committed_sent = true;
        let state = p.state.clone();
        let rid = self.rid();
        for n in state.nodes.keys().filter(|n| **n != self.me.id) {
            out.push(Output::Send {
                to: n.clone(),
                envelope: Envelope::request(
                    COMMIT,
                    self.me.id.clone(),
                    rid,
                    state.version.to_string().into_bytes(),
                ),
            });
        }
        self.apply_committed(state.clone(), durable);
        self.publishing = None;
        if self.notes {
            out.push(self.note(format!(
                "committed v{} nodes={}",
                state.version,
                state.nodes.len()
            )));
        }
        // joins that waited for this publication, and metadata that moved
        if !self.waiting_joins.is_empty() || self.republish_wanted {
            let joins = std::mem::take(&mut self.waiting_joins);
            self.republish_wanted = false;
            let mut s = self.committed.next();
            for j in joins {
                s.nodes.insert(j.id.clone(), j);
            }
            self.place(&mut s, &NoClock);
            out.extend(self.publish(s, &NoClock, durable));
        }
        out
    }

    fn apply_committed(&mut self, state: ClusterState, durable: &mut Durable) {
        if state.version < self.committed.version {
            return;
        }
        self.committed = state;
        self.accepted = None;
        durable.entries.insert(
            "cluster_state".into(),
            serde_json::to_vec(&self.committed).unwrap_or_default(),
        );
    }

    // ---- the follower side ------------------------------------------------------------

    fn look_for_manager(&mut self) -> Vec<Output> {
        let mut out = Vec::new();
        let targets: Vec<NodeId> = match &self.configured_manager {
            Some(m) => vec![m.clone()],
            None => self.seeds.clone(),
        };
        let rid = self.rid();
        let body = serde_json::to_vec(&self.me).unwrap_or_default();
        for t in targets {
            if t == self.me.id {
                continue;
            }
            out.push(Output::Send {
                to: t,
                envelope: Envelope::request(JOIN, self.me.id.clone(), rid, body.clone()),
            });
        }
        out.push(Output::Timer { id: T_JOIN, after: self.timings.join_retry });
        out
    }
}

/// A clock for the places that do not read one.
struct NoClock;
impl Clock for NoClock {
    fn now(&self) -> Millis {
        0
    }
    fn wall(&self) -> Millis {
        0
    }
}

impl NodeLogic for Coordinator {
    fn handle(&mut self, input: Input, clock: &dyn Clock, durable: &mut Durable) -> Vec<Output> {
        match input {
            Input::Start => {
                // what was committed before survives a restart
                if let Some(bytes) = durable.entries.get("cluster_state") {
                    if let Ok(s) = serde_json::from_slice::<ClusterState>(bytes) {
                        self.committed = s;
                    }
                }
                if self.i_am_manager() {
                    self.become_leader(clock, durable)
                } else {
                    self.mode = Mode::Candidate;
                    self.look_for_manager()
                }
            }
            Input::Timer(T_JOIN) => {
                if self.mode == Mode::Candidate {
                    self.look_for_manager()
                } else {
                    vec![]
                }
            }
            Input::Timer(T_FOLLOWER_CHECK) => {
                if self.mode != Mode::Leader {
                    return vec![];
                }
                let mut out = Vec::new();
                // unanswered checks count as misses
                let missed: Vec<NodeId> = self.pending_checks.values().cloned().collect();
                self.pending_checks.clear();
                let mut gone = Vec::new();
                for n in missed {
                    let h = self.followers.entry(n.clone()).or_default();
                    h.misses += 1;
                    if h.misses > self.timings.follower_check_retries {
                        gone.push(n);
                    }
                }
                if !gone.is_empty() && self.publishing.is_none() {
                    let mut s = self.committed.next();
                    for n in &gone {
                        s.nodes.remove(n);
                        self.followers.remove(n);
                        if self.notes {
                            out.push(self.note(format!("removed {n}")));
                        }
                    }
                    out.extend(self.publish(s, clock, durable));
                }
                let followers: Vec<NodeId> =
                    self.committed.nodes.keys().filter(|n| **n != self.me.id).cloned().collect();
                for n in followers {
                    let rid = self.rid();
                    self.pending_checks.insert(rid, n.clone());
                    out.push(Output::Send {
                        to: n,
                        envelope: Envelope::request(
                            FOLLOWER_CHECK,
                            self.me.id.clone(),
                            rid,
                            vec![],
                        ),
                    });
                }
                out.push(Output::Timer {
                    id: T_FOLLOWER_CHECK,
                    after: self.timings.follower_check_interval,
                });
                out
            }
            Input::Timer(T_METADATA) => {
                if self.mode != Mode::Leader {
                    return vec![];
                }
                let mut out = Vec::new();
                let changed = self
                    .metadata
                    .as_ref()
                    .map(|m| super::metadata::fingerprint(&m.snapshot()) != self.last_fingerprint)
                    .unwrap_or(false);
                if changed {
                    if self.publishing.is_some() {
                        self.republish_wanted = true;
                    } else {
                        let mut s = self.committed.next();
                        self.place(&mut s, clock);
                        if self.notes {
                            out.push(
                                self.note(format!("metadata changed: {} indices", s.indices.len())),
                            );
                        }
                        out.extend(self.publish(s, clock, durable));
                    }
                }
                out.push(Output::Timer { id: T_METADATA, after: self.metadata_poll });
                out
            }
            Input::Timer(T_LEADER_CHECK) => {
                let Mode::Follower(leader) = self.mode.clone() else { return vec![] };
                let mut out = Vec::new();
                if self.leader_check_outstanding.is_some() {
                    self.leader_misses += 1;
                    if self.leader_misses > self.timings.leader_check_retries {
                        // the manager is gone: look again
                        self.mode = Mode::Candidate;
                        self.leader_misses = 0;
                        self.leader_check_outstanding = None;
                        if self.notes {
                            out.push(self.note(format!("lost manager {leader}")));
                        }
                        out.extend(self.look_for_manager());
                        return out;
                    }
                }
                let rid = self.rid();
                self.leader_check_outstanding = Some(rid);
                out.push(Output::Send {
                    to: leader,
                    envelope: Envelope::request(LEADER_CHECK, self.me.id.clone(), rid, vec![]),
                });
                out.push(Output::Timer {
                    id: T_LEADER_CHECK,
                    after: self.timings.leader_check_interval,
                });
                out
            }
            Input::Timer(_) => vec![],
            Input::Message(e) => self.on_message(e, clock, durable),
        }
    }
}

impl Coordinator {
    fn on_message(&mut self, e: Envelope, clock: &dyn Clock, durable: &mut Durable) -> Vec<Output> {
        match (e.action.as_str(), e.kind) {
            (JOIN, Kind::Request) => {
                if self.mode != Mode::Leader {
                    // not the manager: say who is, if known
                    let who = match &self.mode {
                        Mode::Follower(l) => json!({"manager": l.as_str()}),
                        _ => json!({"manager": null}),
                    };
                    return vec![Output::Send {
                        to: e.from.clone(),
                        envelope: e.error(self.me.id.clone(), &who.to_string()),
                    }];
                }
                let Ok(node) = serde_json::from_slice::<DiscoveryNode>(&e.body) else {
                    return vec![];
                };
                let mut out = Vec::new();
                if self.committed.nodes.contains_key(&node.id)
                    && self.committed.nodes[&node.id] == node
                {
                    // already in: answer with the state
                    let body = serde_json::to_vec(&self.committed).unwrap_or_default();
                    out.push(Output::Send {
                        to: e.from.clone(),
                        envelope: e.response(self.me.id.clone(), body),
                    });
                    return out;
                }
                self.followers.entry(node.id.clone()).or_default().misses = 0;
                if self.publishing.is_some() {
                    self.waiting_joins.push(node);
                    return out;
                }
                let mut s = self.committed.next();
                s.nodes.insert(node.id.clone(), node.clone());
                self.place(&mut s, clock);
                if self.notes {
                    out.push(self.note(format!("join {}", node.id)));
                }
                out.extend(self.publish(s, clock, durable));
                out
            }
            (JOIN, Kind::Error) => {
                // pointed at another manager
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                if let Some(m) = v.get("manager").and_then(|m| m.as_str()) {
                    self.configured_manager = Some(NodeId(m.to_string()));
                }
                vec![]
            }
            (JOIN, Kind::Response) => {
                if let Ok(s) = serde_json::from_slice::<ClusterState>(&e.body) {
                    self.apply_committed(s, durable);
                    self.mode = Mode::Follower(e.from.clone());
                    return vec![Output::Timer {
                        id: T_LEADER_CHECK,
                        after: self.timings.leader_check_interval,
                    }];
                }
                vec![]
            }
            (PUBLISH, Kind::Request) => {
                let Ok(s) = serde_json::from_slice::<ClusterState>(&e.body) else { return vec![] };
                if s.version <= self.committed.version && s.state_uuid != self.committed.state_uuid
                {
                    // older than what is committed: refuse
                    return vec![Output::Send {
                        to: e.from.clone(),
                        envelope: e.error(self.me.id.clone(), "stale"),
                    }];
                }
                let leader = s.cluster_manager.clone().unwrap_or_else(|| e.from.clone());
                self.accepted = Some(s);
                let was_candidate = self.mode == Mode::Candidate;
                self.mode = Mode::Follower(leader);
                let mut out = vec![Output::Send {
                    to: e.from.clone(),
                    envelope: e.response(self.me.id.clone(), vec![]),
                }];
                if was_candidate {
                    self.leader_misses = 0;
                    self.leader_check_outstanding = None;
                    out.push(Output::Timer {
                        id: T_LEADER_CHECK,
                        after: self.timings.leader_check_interval,
                    });
                }
                out
            }
            (PUBLISH, Kind::Response) => {
                if self.mode != Mode::Leader {
                    return vec![];
                }
                let all = self.publishing.as_mut().map(|p| {
                    p.acked.insert(e.from.clone());
                    p.state.nodes.keys().all(|n| p.acked.contains(n))
                });
                if all == Some(true) { self.commit_publication(durable) } else { vec![] }
            }
            (PUBLISH, Kind::Error) => {
                // a node that refused: drop it from the publication so it can commit
                if let Some(p) = self.publishing.as_mut() {
                    p.state.nodes.remove(&e.from);
                    self.followers.remove(&e.from);
                    let all = p.state.nodes.keys().all(|n| p.acked.contains(n));
                    if all {
                        return self.commit_publication(durable);
                    }
                }
                vec![]
            }
            (COMMIT, Kind::Request) => {
                let version: u64 = String::from_utf8_lossy(&e.body).parse().unwrap_or(0);
                if let Some(a) = self.accepted.clone() {
                    if a.version == version {
                        self.apply_committed(a, durable);
                        let mut out = vec![Output::Send {
                            to: e.from.clone(),
                            envelope: e.response(self.me.id.clone(), vec![]),
                        }];
                        if self.notes {
                            out.push(self.note(format!(
                                "committed v{version} nodes={}",
                                self.committed.nodes.len()
                            )));
                        }
                        return out;
                    }
                }
                vec![]
            }
            (FOLLOWER_CHECK, Kind::Request) => {
                // a follower answers its manager; anyone else is not our manager
                match &self.mode {
                    Mode::Follower(l) if *l == e.from => {
                        vec![Output::Send {
                            to: e.from.clone(),
                            envelope: e.response(self.me.id.clone(), vec![]),
                        }]
                    }
                    Mode::Candidate => vec![Output::Send {
                        to: e.from.clone(),
                        envelope: e.response(self.me.id.clone(), vec![]),
                    }],
                    _ => vec![Output::Send {
                        to: e.from.clone(),
                        envelope: e.error(self.me.id.clone(), "not my manager"),
                    }],
                }
            }
            (FOLLOWER_CHECK, Kind::Response) => {
                if let Some(n) = self.pending_checks.remove(&e.request_id) {
                    if let Some(h) = self.followers.get_mut(&n) {
                        h.misses = 0;
                    }
                }
                vec![]
            }
            (LEADER_CHECK, Kind::Request) => {
                if self.mode == Mode::Leader && self.committed.nodes.contains_key(&e.from) {
                    vec![Output::Send {
                        to: e.from.clone(),
                        envelope: e.response(self.me.id.clone(), vec![]),
                    }]
                } else {
                    vec![Output::Send {
                        to: e.from.clone(),
                        envelope: e.error(self.me.id.clone(), "not the manager"),
                    }]
                }
            }
            (LEADER_CHECK, Kind::Response) => {
                if self.leader_check_outstanding == Some(e.request_id) {
                    self.leader_check_outstanding = None;
                    self.leader_misses = 0;
                }
                vec![]
            }
            (LEADER_CHECK, Kind::Error) => {
                // the node we follow says it is not the manager: look again
                self.mode = Mode::Candidate;
                self.leader_check_outstanding = None;
                self.look_for_manager()
            }
            (LEAVE, Kind::Request) => {
                if self.mode == Mode::Leader && self.publishing.is_none() {
                    let mut s = self.committed.next();
                    s.nodes.remove(&e.from);
                    self.followers.remove(&e.from);
                    let mut out = vec![Output::Send {
                        to: e.from.clone(),
                        envelope: e.response(self.me.id.clone(), vec![]),
                    }];
                    out.extend(self.publish(s, clock, durable));
                    return out;
                }
                vec![]
            }
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::sim::Sim;

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

    fn cluster(seed: u64, names: &[&str], manager: &str) -> (Sim, Vec<NodeId>) {
        let mut sim = Sim::new(seed);
        let ids: Vec<NodeId> = names.iter().map(|n| NodeId((*n).into())).collect();
        for n in names {
            let me = node(n);
            let m = NodeId(manager.into());
            sim.add_node(
                me.id.clone(),
                Box::new(move |_| {
                    Box::new(Coordinator::new(
                        node(&me.name),
                        "c",
                        "cluster-uuid",
                        Some(m.clone()),
                        vec![],
                    ))
                }),
            );
        }
        (sim, ids)
    }

    /// The committed state of every node that is up, by version.
    fn versions(sim: &Sim, ids: &[NodeId]) -> Vec<(String, u64, usize)> {
        ids.iter()
            .filter(|id| sim.is_up(id))
            .filter_map(|id| {
                // a node that has not committed anything yet has nothing to say
                let bytes = sim.durable(id)?.entries.get("cluster_state")?;
                let s: ClusterState = serde_json::from_slice(bytes).ok()?;
                Some((id.0.clone(), s.version, s.nodes.len()))
            })
            .collect()
    }

    #[test]
    fn three_nodes_join_and_agree() {
        let (mut sim, ids) = cluster(1, &["m", "a", "b"], "m");
        sim.run_until(5_000);
        let v = versions(&sim, &ids);
        assert!(v.iter().all(|(_, _, n)| *n == 3), "{v:?}");
        let versions_seen: BTreeSet<u64> = v.iter().map(|(_, ver, _)| *ver).collect();
        assert_eq!(versions_seen.len(), 1, "{v:?}");
        // the invariant every later step keeps: one version, one state, on every node
        let states: BTreeSet<Vec<u8>> = ids
            .iter()
            .map(|id| sim.durable(id).unwrap().entries.get("cluster_state").unwrap().clone())
            .collect();
        assert_eq!(states.len(), 1, "nodes committed different states at the same version");
    }

    #[test]
    fn a_partitioned_follower_is_dropped_and_comes_back() {
        let (mut sim, ids) = cluster(2, &["m", "a", "b"], "m");
        sim.run_until(5_000);
        let (m, a) = (ids[0].clone(), ids[1].clone());
        sim.partition(&[a.clone()], &[m.clone(), ids[2].clone()]);
        sim.run_until(30_000);
        let v = versions(&sim, &ids);
        let manager_view = v.iter().find(|(n, _, _)| n == "m").unwrap();
        assert_eq!(manager_view.2, 2, "manager should have dropped a: {v:?}");
        assert!(
            sim.notes.iter().any(|(_, n, t)| *n == a && t.starts_with("lost manager")),
            "{:?}",
            sim.notes
        );
        sim.heal();
        sim.run_until(60_000);
        let v = versions(&sim, &ids);
        assert!(v.iter().all(|(_, _, n)| *n == 3), "{v:?}");
        let versions_seen: BTreeSet<u64> = v.iter().map(|(_, ver, _)| *ver).collect();
        assert_eq!(versions_seen.len(), 1, "{v:?}");
    }

    #[test]
    fn a_follower_that_crashes_rejoins_from_what_it_kept() {
        let (mut sim, ids) = cluster(3, &["m", "a", "b"], "m");
        sim.run_until(5_000);
        let b = ids[2].clone();
        let before = versions(&sim, &[b.clone()])[0].1;
        sim.crash(&b);
        sim.run_until(30_000);
        assert_eq!(versions(&sim, &[ids[0].clone()])[0].2, 2);
        sim.restart(&b);
        sim.run_until(60_000);
        let v = versions(&sim, &ids);
        assert!(v.iter().all(|(_, _, n)| *n == 3), "{v:?}");
        assert!(v.iter().find(|(n, _, _)| n == "b").unwrap().1 > before);
    }

    #[test]
    fn index_metadata_is_published_when_it_changes_and_only_then() {
        use crate::cluster::metadata::MapSource;
        use crate::cluster::state::{IndexMetadata, ShardState};
        let source = std::sync::Arc::new(MapSource(parking_lot::Mutex::new(BTreeMap::new())));
        let mut sim = Sim::new(5);
        let ids: Vec<NodeId> = ["m", "a"].iter().map(|n| NodeId((*n).into())).collect();
        for n in ["m", "a"] {
            let src = source.clone();
            let m = NodeId("m".into());
            sim.add_node(
                NodeId(n.into()),
                Box::new(move |_| {
                    let mut c = Coordinator::new(node(n), "c", "u", Some(m.clone()), vec![]);
                    if n == "m" {
                        c.metadata = Some(src.clone());
                    }
                    Box::new(c)
                }),
            );
        }
        sim.run_until(5_000);
        let v0 = versions(&sim, &ids);
        // nothing changed: no new versions while the manager polls
        sim.run_until(15_000);
        assert_eq!(versions(&sim, &ids), v0, "version churn with nothing changed");
        // an index appears
        source.0.lock().insert(
            "logs".into(),
            IndexMetadata {
                name: "logs".into(),
                uuid: "uuid1".into(),
                version: 1,
                mapping_version: 1,
                settings_version: 1,
                aliases_version: 1,
                state: "open".into(),
                settings: json!({"index": {"number_of_shards": "2", "number_of_replicas": "1"}}),
                mappings: json!({}),
                aliases: json!({}),
                number_of_shards: 2,
                number_of_replicas: 1,
                primary_terms: BTreeMap::new(),
                in_sync_allocations: BTreeMap::new(),
                creation_date: 0,
            },
        );
        sim.run_until(25_000);
        for id in &ids {
            let s: ClusterState = serde_json::from_slice(
                sim.durable(id).unwrap().entries.get("cluster_state").unwrap(),
            )
            .unwrap();
            assert!(s.indices.contains_key("logs"), "{id} has no logs");
            let copies: Vec<_> = s.routing.shards_of("logs").collect();
            assert_eq!(copies.len(), 4);
            assert!(
                copies.iter().filter(|c| c.primary).all(|c| c.state == ShardState::Started
                    && c.node.as_ref() == Some(&NodeId("m".into())))
            );
            assert!(
                copies
                    .iter()
                    .filter(|c| !c.primary)
                    .all(|c| c.state == ShardState::Unassigned && c.node.is_none())
            );
            assert_eq!(s.indices["logs"].in_sync_allocations[&0].len(), 1);
        }
        // and goes away
        source.0.lock().clear();
        sim.run_until(35_000);
        for id in &ids {
            let s: ClusterState = serde_json::from_slice(
                sim.durable(id).unwrap().entries.get("cluster_state").unwrap(),
            )
            .unwrap();
            assert!(s.indices.is_empty(), "{id} still has indices");
        }
    }

    #[test]
    fn versions_only_rise_and_the_seed_repeats() {
        let (mut x, ids) = cluster(9, &["m", "a", "b", "c"], "m");
        x.network.drop_rate = 0.2;
        let mut last: BTreeMap<String, u64> = BTreeMap::new();
        for t in (1_000..=40_000).step_by(1_000) {
            x.run_until(t);
            for (n, v, _) in versions(&x, &ids) {
                let prev = last.insert(n.clone(), v).unwrap_or(0);
                assert!(v >= prev, "{n} went from {prev} to {v}");
            }
        }
        let (mut y, _) = cluster(9, &["m", "a", "b", "c"], "m");
        y.network.drop_rate = 0.2;
        y.run_until(40_000);
        assert_eq!(x.trace, y.trace);
    }
}
