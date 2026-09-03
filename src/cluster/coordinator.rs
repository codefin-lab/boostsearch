//! The cluster manager is elected, and what it publishes is committed
//! once a quorum has accepted it: OpenSearch's coordination, as one
//! `NodeLogic`, so it runs the same under the simulation and under the
//! production runtime.
//!
//! A node keeps three things on disk: the term it is in, the last state
//! it accepted, and the last state it committed. A candidate finds its
//! peers, asks them for a pre-vote (which changes nothing) and, once a
//! quorum of the voting configuration would vote for it and none of them
//! has accepted anything fresher, starts an election: it picks a term
//! above every term it has seen and tells everyone to join it in that
//! term. A node told to join moves to the term and answers with what it
//! last accepted; the candidate counts the joins from nodes whose
//! accepted state is no fresher than its own, and with a quorum of both
//! the committed and the accepted configuration it is the manager.
//!
//! The manager publishes states in two phases. Every node in the state
//! is sent it; a node accepts it when it is in the manager's term and
//! newer than what it accepted before; once a quorum of both
//! configurations has accepted, the manager commits and tells the rest
//! to. A publication that does not reach a quorum in time makes the
//! manager step down. The voting configuration follows the nodes: with
//! `cluster.auto_shrink_voting_configuration` it is the largest odd
//! number of live manager-eligible nodes, never below three unless nodes
//! are excluded, and it changes one step at a time, each step committed
//! by a quorum of the configuration before it and the one after.
//!
//! The manager checks its followers on a timer and drops one that stops
//! answering; a follower that stops hearing from its manager becomes a
//! candidate again. Every term is only ever led by one node, and two
//! nodes that committed the same term and version committed the same
//! state -- the simulation tests below hold the cluster to both.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::clock::{Clock, Millis};
use super::sim::{Durable, Input, NodeLogic, Output, Rng};
use super::state::{ClusterState, DiscoveryNode};
use super::transport::{Envelope, Kind, NodeId};

pub const PEERS: &str = "internal:discovery/request_peers";
pub const PRE_VOTE: &str = "internal:cluster/request_pre_vote";
pub const START_JOIN: &str = "internal:cluster/coordination/start_join";
pub const JOIN: &str = "internal:cluster/coordination/join";
pub const PUBLISH: &str = "internal:cluster/coordination/publish_state";
pub const COMMIT: &str = "internal:cluster/coordination/commit_state";
pub const FOLLOWER_CHECK: &str = "internal:coordination/fault_detection/follower_check";
pub const LEADER_CHECK: &str = "internal:coordination/fault_detection/leader_check";
pub const LEAVE: &str = "internal:cluster/coordination/leave";
pub const SHARD_STARTED: &str = "internal:cluster/shard/started";
pub const SHARD_FAILED: &str = "internal:cluster/shard/failure";
/// a copy that was in sync missed an acknowledged write: it is in sync no more
pub const SHARD_STALE: &str = "internal:cluster/shard/stale";
pub const REROUTE: &str = "internal:cluster/reroute";
/// a node holding an index's primary tells the manager the index's metadata
pub const METADATA_REPORT: &str = "internal:cluster/metadata/report";

/// What a node keeps across a restart, by durable key.
pub const D_COMMITTED: &str = "cluster_state";
pub const D_ACCEPTED: &str = "accepted_state";
pub const D_TERM: &str = "current_term";

/// Timers, by id.
const T_DISCOVER: u64 = 1;
const T_FOLLOWER_CHECK: u64 = 2;
const T_LEADER_CHECK: u64 = 3;
const T_METADATA: u64 = 4;
const T_ELECTION: u64 = 5;
const T_PUBLISH: u64 = 6;
/// a replica's wait for its departed node is over
const T_ALLOCATE: u64 = 7;

/// The timings, in milliseconds; OpenSearch's defaults where it has them.
#[derive(Clone, Debug)]
pub struct Timings {
    /// `discovery.find_peers_interval`
    pub discover_interval: Millis,
    /// `cluster.election.initial_timeout`, `back_off_time`, `max_timeout`
    pub election_initial: Millis,
    pub election_backoff: Millis,
    pub election_max: Millis,
    /// `cluster.fault_detection.follower_check.*`
    pub follower_check_interval: Millis,
    pub follower_check_retries: u32,
    /// `cluster.fault_detection.leader_check.*`
    pub leader_check_interval: Millis,
    pub leader_check_retries: u32,
    /// `cluster.publish.timeout`
    pub publish_timeout: Millis,
}

impl Default for Timings {
    fn default() -> Timings {
        Timings {
            discover_interval: 1_000,
            election_initial: 100,
            election_backoff: 100,
            election_max: 10_000,
            follower_check_interval: 1_000,
            follower_check_retries: 3,
            leader_check_interval: 1_000,
            leader_check_retries: 3,
            publish_timeout: 30_000,
        }
    }
}

/// What a node believes about the cluster right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// looking for a manager, or trying to become one
    Candidate,
    /// following the named manager
    Follower(NodeId),
    /// the manager
    Leader,
}

pub struct Coordinator {
    pub me: DiscoveryNode,
    pub timings: Timings,
    /// `cluster.initial_cluster_manager_nodes`: the first voting configuration
    pub initial_names: Vec<String>,
    /// nodes to ask first when looking for peers
    pub seeds: Vec<NodeId>,
    /// `discovery.seed_hosts`: addresses dialled until the node behind each is known
    pub seed_hosts: Vec<String>,
    pub mode: Mode,
    /// the term this node is in; on disk
    pub current_term: u64,
    /// the state this node has committed; on disk
    pub committed: ClusterState,
    /// the last state this node accepted; on disk, never older than committed
    pub accepted: ClusterState,
    /// `cluster.auto_shrink_voting_configuration`
    pub auto_shrink: bool,
    /// every node this one has heard of
    peers: BTreeMap<NodeId, DiscoveryNode>,
    /// who the peers say the manager is
    leader_hint: Option<NodeId>,
    rng: Rng,
    election_attempt: u32,
    election_scheduled: bool,
    /// the pre-vote round in flight, by request id, and its answers
    prevote_rid: u64,
    prevotes: BTreeMap<NodeId, (u64, u64, u64)>,
    max_term_seen: u64,
    /// joins received in the current term that carry a vote
    join_votes: BTreeMap<NodeId, DiscoveryNode>,
    /// the start-join this node answered: its one vote in the term goes there
    last_join: Option<(u64, NodeId)>,
    /// the manager's own bookkeeping
    followers: BTreeMap<NodeId, FollowerHealth>,
    publishing: Option<Publication>,
    publish_seq: u64,
    /// nodes that did not accept the last publication
    lagging: BTreeSet<NodeId>,
    next_request: u64,
    /// unanswered follower checks, by request id
    pending_checks: BTreeMap<u64, NodeId>,
    leader_misses: u32,
    leader_check_outstanding: Option<u64>,
    /// nodes that asked to join, and to leave, while a publication was in flight
    waiting_joins: BTreeMap<NodeId, DiscoveryNode>,
    pending_removals: BTreeSet<NodeId>,
    pub notes: bool,
    /// where the manager reads index metadata and exclusions, and how often
    pub metadata: Option<std::sync::Arc<dyn super::metadata::MetadataSource>>,
    pub metadata_poll: Millis,
    last_fingerprint: u64,
    /// where this node keeps the copies the manager puts on it
    pub host: Option<std::sync::Arc<dyn super::metadata::ShardHost>>,
    /// copies this node has reported started or failed, by allocation id
    reported: BTreeSet<String>,
    /// copies this node holds for the manager, by allocation id
    hosted: BTreeMap<String, (String, u32)>,
    /// what data nodes reported since the last publication
    shard_events: Vec<ShardEvent>,
    /// primary terms, raised when a replica takes over
    terms: BTreeMap<(String, u32), u64>,
    /// a table `_cluster/reroute` commands shaped, for the next publication
    command_table: Option<super::state::RoutingTable>,
    /// index metadata the nodes holding primaries reported, by index
    reports: BTreeMap<String, (NodeId, super::state::IndexMetadata)>,
    /// what this node last reported about its own primaries, by index
    reported_fp: BTreeMap<String, u64>,
    /// the customs this node last applied, as a fingerprint
    customs_applied: u64,
    /// the index copies each node holds on disk, as the nodes reported them
    held: BTreeMap<NodeId, Vec<(String, String, String)>>,
    /// what this node last reported holding
    held_reported: Vec<(String, String, String)>,
    /// `_cluster/voting_config_exclusions`, as (node id, node name)
    exclusions: Vec<(String, String)>,
    /// a publication asked for while one was in flight
    republish_wanted: bool,
    /// the wall time when this node was last handed an input
    last_wall: Millis,
}

/// A data node's word about a copy the manager put on it.
#[derive(Clone, Debug)]
enum ShardEvent {
    Started { index: String, shard: u32, allocation_id: String },
    Failed { index: String, shard: u32, allocation_id: String, message: String },
    Stale { index: String, shard: u32, allocation_id: String },
}

#[derive(Clone, Debug, Default)]
struct FollowerHealth {
    misses: u32,
}

#[derive(Clone, Debug)]
struct Publication {
    state: ClusterState,
    rid: u64,
    seq: u64,
    acked: BTreeSet<NodeId>,
}

/// A majority of a configuration is among the votes.
fn has_quorum(config: &[NodeId], votes: &BTreeSet<NodeId>) -> bool {
    !config.is_empty() && config.iter().filter(|n| votes.contains(n)).count() * 2 > config.len()
}

/// A quorum of both the committed and the accepted configuration.
fn election_quorum(state: &ClusterState, votes: &BTreeSet<NodeId>) -> bool {
    has_quorum(&state.last_committed_config, votes)
        && has_quorum(&state.last_accepted_config, votes)
}

/// `(term, version)` ordering of states.
fn fresher(a_term: u64, a_version: u64, b_term: u64, b_version: u64) -> bool {
    a_term > b_term || (a_term == b_term && a_version > b_version)
}

fn seed_of(id: &NodeId) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.as_str().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

impl Coordinator {
    pub fn new(
        me: DiscoveryNode,
        cluster_name: &str,
        cluster_uuid: &str,
        initial_names: Vec<String>,
        seeds: Vec<NodeId>,
    ) -> Coordinator {
        let rng = Rng::new(seed_of(&me.id));
        let committed = ClusterState::empty(cluster_name, cluster_uuid);
        Coordinator {
            me,
            timings: Timings::default(),
            initial_names,
            seeds,
            seed_hosts: Vec::new(),
            mode: Mode::Candidate,
            current_term: 0,
            accepted: committed.clone(),
            committed,
            auto_shrink: true,
            peers: BTreeMap::new(),
            leader_hint: None,
            rng,
            election_attempt: 0,
            election_scheduled: false,
            prevote_rid: 0,
            prevotes: BTreeMap::new(),
            max_term_seen: 0,
            join_votes: BTreeMap::new(),
            last_join: None,
            followers: BTreeMap::new(),
            publishing: None,
            publish_seq: 0,
            lagging: BTreeSet::new(),
            next_request: 1,
            pending_checks: BTreeMap::new(),
            leader_misses: 0,
            leader_check_outstanding: None,
            waiting_joins: BTreeMap::new(),
            pending_removals: BTreeSet::new(),
            notes: true,
            metadata: None,
            metadata_poll: 500,
            last_fingerprint: 0,
            host: None,
            reported: BTreeSet::new(),
            hosted: BTreeMap::new(),
            shard_events: Vec::new(),
            terms: BTreeMap::new(),
            command_table: None,
            reports: BTreeMap::new(),
            reported_fp: BTreeMap::new(),
            customs_applied: 0,
            held: BTreeMap::new(),
            held_reported: Vec::new(),
            exclusions: Vec::new(),
            republish_wanted: false,
            last_wall: 0,
        }
    }

    /// The committed state, as `_cluster/state` reads it.
    pub fn state(&self) -> &ClusterState {
        &self.committed
    }

    /// What the nodes told this manager they hold (tests).
    pub fn held_report(&self) -> BTreeMap<NodeId, Vec<(String, String, String)>> {
        self.held.clone()
    }

    fn eligible(&self) -> bool {
        self.me.is_cluster_manager_eligible()
    }

    fn rid(&mut self) -> u64 {
        self.next_request += 1;
        self.next_request
    }

    fn note(&self, text: String) -> Output {
        Output::Note(text)
    }

    fn send(&self, to: &NodeId, envelope: Envelope) -> Output {
        Output::Send { to: to.clone(), envelope }
    }

    fn request(&mut self, to: &NodeId, action: &str, body: Value) -> Output {
        let rid = self.rid();
        let bytes = serde_json::to_vec(&body).unwrap_or_default();
        self.send(to, Envelope::request(action, self.me.id.clone(), rid, bytes))
    }

    // ---- what is kept on disk ------------------------------------------------------

    fn persist_term(&self, durable: &mut Durable) {
        durable.entries.insert(D_TERM.into(), self.current_term.to_string().into_bytes());
    }

    fn persist_accepted(&self, durable: &mut Durable) {
        durable
            .entries
            .insert(D_ACCEPTED.into(), serde_json::to_vec(&self.accepted).unwrap_or_default());
    }

    fn persist_committed(&self, durable: &mut Durable) {
        durable
            .entries
            .insert(D_COMMITTED.into(), serde_json::to_vec(&self.committed).unwrap_or_default());
    }

    /// A node this one has learned of: remembered, and told to the transport.
    fn learn(&mut self, node: DiscoveryNode) -> Vec<Output> {
        if node.id == self.me.id {
            return vec![];
        }
        let known = self.peers.get(&node.id) == Some(&node);
        if known {
            return vec![];
        }
        let out =
            vec![Output::Peer { id: node.id.clone(), address: node.transport_address.clone() }];
        self.peers.insert(node.id.clone(), node);
        out
    }

    /// Everyone this node could talk to, itself excluded.
    fn everyone(&self) -> BTreeSet<NodeId> {
        let mut all: BTreeSet<NodeId> = self.peers.keys().cloned().collect();
        all.extend(self.seeds.iter().cloned());
        all.extend(self.accepted.nodes.keys().cloned());
        all.extend(self.accepted.last_committed_config.iter().cloned());
        all.extend(self.accepted.last_accepted_config.iter().cloned());
        all.remove(&self.me.id);
        all
    }

    /// A higher term seen anywhere: move to it, and stop leading or
    /// following anything from an older one.
    fn saw_term(&mut self, term: u64, durable: &mut Durable) -> Vec<Output> {
        self.max_term_seen = self.max_term_seen.max(term);
        if term <= self.current_term {
            return vec![];
        }
        self.current_term = term;
        self.persist_term(durable);
        self.join_votes.clear();
        if self.mode != Mode::Candidate {
            let mut out = Vec::new();
            if self.notes {
                out.push(self.note(format!("term {term}: stepping down")));
            }
            out.extend(self.become_candidate());
            return out;
        }
        vec![]
    }

    // ---- being a candidate -------------------------------------------------------

    fn become_candidate(&mut self) -> Vec<Output> {
        self.mode = Mode::Candidate;
        self.publishing = None;
        self.followers.clear();
        self.pending_checks.clear();
        self.leader_check_outstanding = None;
        self.leader_misses = 0;
        self.election_attempt = 0;
        self.election_scheduled = false;
        self.leader_hint = None;
        self.discover()
    }

    /// The first voting configuration, once every node named for it is known.
    fn bootstrap(&mut self, durable: &mut Durable) -> Vec<Output> {
        if !self.accepted.last_accepted_config.is_empty() || !self.eligible() {
            return vec![];
        }
        let names_me =
            self.initial_names.iter().any(|n| *n == self.me.name || *n == self.me.id.as_str());
        if !names_me {
            return vec![];
        }
        let mut ids = Vec::new();
        for name in &self.initial_names {
            let found = if *name == self.me.name || *name == self.me.id.as_str() {
                Some(self.me.id.clone())
            } else {
                self.peers
                    .values()
                    .find(|p| p.name == *name || p.id.as_str() == name)
                    .map(|p| p.id.clone())
            };
            match found {
                Some(id) => ids.push(id),
                None => return vec![],
            }
        }
        ids.sort();
        ids.dedup();
        self.accepted.last_accepted_config = ids.clone();
        self.accepted.last_committed_config = ids.clone();
        self.committed.last_accepted_config = ids.clone();
        self.committed.last_committed_config = ids.clone();
        self.persist_accepted(durable);
        self.persist_committed(durable);
        let mut out = Vec::new();
        if self.notes {
            let names: Vec<&str> = ids.iter().map(|i| i.as_str()).collect();
            out.push(self.note(format!("bootstrapped {names:?}")));
        }
        out
    }

    /// Ask around for peers and for the manager; try to join one that is
    /// known; get an election on the calendar if there is none.
    fn discover(&mut self) -> Vec<Output> {
        if self.mode != Mode::Candidate {
            return vec![];
        }
        let mut out = Vec::new();
        // a seed host whose node is not known yet is dialled again
        let known: BTreeSet<&str> =
            self.peers.values().map(|p| p.transport_address.as_str()).collect();
        for h in &self.seed_hosts {
            if !known.contains(h.as_str()) && *h != self.me.transport_address {
                out.push(Output::Dial(h.clone()));
            }
        }
        let body = json!({"node": self.me});
        for t in self.everyone() {
            out.push(self.request(&t, PEERS, body.clone()));
        }
        if let Some(l) = self.leader_hint.clone() {
            out.push(self.join(&l));
        }
        if self.eligible()
            && !self.accepted.last_accepted_config.is_empty()
            && !self.election_scheduled
        {
            out.push(self.schedule_election());
        }
        out.push(Output::Timer { id: T_DISCOVER, after: self.timings.discover_interval });
        out
    }

    /// A join carries a vote only when it answers the start-join of the
    /// node it goes to, in this term: one vote per node per term.
    fn join_body(&self, to: &NodeId) -> Value {
        let vote = self.last_join.as_ref() == Some(&(self.current_term, to.clone()));
        let held = self.metadata.as_ref().map(|m| m.held()).unwrap_or_default();
        json!({
            "term": self.current_term,
            "last_accepted_term": self.accepted.term,
            "last_accepted_version": self.accepted.version,
            "node": self.me,
            "vote": vote,
            "held": held,
        })
    }

    fn join(&mut self, to: &NodeId) -> Output {
        let body = self.join_body(to);
        self.request(to, JOIN, body)
    }

    /// An election attempt after a randomised, growing delay, as
    /// `cluster.election.*` describe it.
    fn schedule_election(&mut self) -> Output {
        self.election_attempt += 1;
        let t = &self.timings;
        let cap = t
            .election_max
            .min(t.election_initial + t.election_backoff * self.election_attempt as u64);
        let after = t.election_initial + self.rng.range(0, cap);
        self.election_scheduled = true;
        Output::Timer { id: T_ELECTION, after }
    }

    /// A pre-vote round: ask every manager-eligible node whether it would
    /// vote, without changing anything.
    fn prevote(&mut self) -> Vec<Output> {
        let mut out = Vec::new();
        self.prevotes.clear();
        self.max_term_seen = self.max_term_seen.max(self.current_term);
        let rid = self.rid();
        self.prevote_rid = rid;
        let body = serde_json::to_vec(&json!({"term": self.current_term})).unwrap_or_default();
        for t in self.everyone() {
            let eligible =
                self.peers.get(&t).map(|p| p.is_cluster_manager_eligible()).unwrap_or(true);
            if eligible {
                out.push(
                    self.send(
                        &t,
                        Envelope::request(PRE_VOTE, self.me.id.clone(), rid, body.clone()),
                    ),
                );
            }
        }
        out
    }

    fn prevote_quorum(&self) -> bool {
        let mut votes: BTreeSet<NodeId> = self.prevotes.keys().cloned().collect();
        votes.insert(self.me.id.clone());
        election_quorum(&self.accepted, &votes)
    }

    /// Start an election: a term above every one seen, and everyone told to join it.
    fn start_election(&mut self, durable: &mut Durable) -> Vec<Output> {
        let term = self.max_term_seen.max(self.current_term) + 1;
        // the pre-vote round is over: late answers start nothing
        self.prevote_rid = 0;
        self.prevotes.clear();
        let mut out = Vec::new();
        if self.notes {
            out.push(self.note(format!("election term {term}")));
        }
        let body = json!({"term": term});
        for t in self.everyone() {
            out.push(self.request(&t, START_JOIN, body.clone()));
        }
        let me = self.me.id.clone();
        out.extend(self.on_start_join(term, &me, durable));
        out
    }

    /// Told to join a term: move to it, and send the join.
    fn on_start_join(&mut self, term: u64, from: &NodeId, durable: &mut Durable) -> Vec<Output> {
        if term <= self.current_term {
            return vec![];
        }
        let mut out = self.saw_term(term, durable);
        self.last_join = Some((term, from.clone()));
        if *from == self.me.id {
            let body = self.join_body(&self.me.id.clone());
            out.extend(self.on_join(body, &self.me.id.clone(), durable));
        } else {
            out.push(self.join(from));
        }
        out
    }

    /// A join: a vote for a candidate, a node for a manager.
    fn on_join(&mut self, body: Value, from: &NodeId, durable: &mut Durable) -> Vec<Output> {
        let term = body.get("term").and_then(|t| t.as_u64()).unwrap_or(0);
        let la_term = body.get("last_accepted_term").and_then(|t| t.as_u64()).unwrap_or(0);
        let la_version = body.get("last_accepted_version").and_then(|t| t.as_u64()).unwrap_or(0);
        let vote = body.get("vote").and_then(|v| v.as_bool()).unwrap_or(false);
        if let Some(h) = body
            .get("held")
            .and_then(|h| serde_json::from_value::<Vec<(String, String, String)>>(h.clone()).ok())
        {
            if let Some(n) = body.get("node").and_then(|n| n.get("id")).and_then(|i| i.as_str()) {
                self.held.insert(NodeId(n.to_string()), h);
            }
        }
        let Some(node) =
            body.get("node").and_then(|n| serde_json::from_value::<DiscoveryNode>(n.clone()).ok())
        else {
            return vec![];
        };
        let mut out = self.saw_term(term, durable);
        out.extend(self.learn(node.clone()));
        if term < self.current_term {
            // behind: a manager brings it up to the term, anyone else says which
            if self.mode == Mode::Leader {
                out.push(self.request(from, START_JOIN, json!({"term": self.current_term})));
            } else {
                let who = match &self.mode {
                    Mode::Follower(l) => Some(l.as_str().to_string()),
                    _ => None,
                };
                let msg = json!({"term": self.current_term, "leader": who}).to_string();
                let rid = self.rid();
                out.push(
                    self.send(
                        from,
                        Envelope::request(JOIN, self.me.id.clone(), rid, vec![])
                            .error(self.me.id.clone(), &msg),
                    ),
                );
            }
            return out;
        }
        if fresher(la_term, la_version, self.accepted.term, self.accepted.version) {
            // it has accepted something this node has not: it cannot follow this node
            let msg = json!({"term": self.current_term, "reason": "fresher"}).to_string();
            let rid = self.rid();
            out.push(
                self.send(
                    from,
                    Envelope::request(JOIN, self.me.id.clone(), rid, vec![])
                        .error(self.me.id.clone(), &msg),
                ),
            );
            return out;
        }
        match self.mode.clone() {
            Mode::Candidate => {
                if !vote {
                    return out;
                }
                self.join_votes.insert(node.id.clone(), node);
                let mut votes: BTreeSet<NodeId> = self.join_votes.keys().cloned().collect();
                votes.insert(self.me.id.clone());
                if self.eligible() && election_quorum(&self.accepted, &votes) {
                    out.extend(self.become_leader(durable));
                }
            }
            Mode::Leader => {
                if *from == self.me.id {
                    return out;
                }
                self.followers.entry(node.id.clone()).or_default().misses = 0;
                self.pending_removals.remove(&node.id);
                if self.committed.nodes.get(&node.id) == Some(&node)
                    && !self.lagging.contains(&node.id)
                {
                    // already in: answer with the state
                    let bytes = serde_json::to_vec(&self.committed).unwrap_or_default();
                    let rid = self.rid();
                    out.push(
                        self.send(
                            from,
                            Envelope::request(JOIN, self.me.id.clone(), rid, vec![])
                                .response(self.me.id.clone(), bytes),
                        ),
                    );
                    return out;
                }
                self.lagging.remove(&node.id);
                self.waiting_joins.insert(node.id.clone(), node);
                out.extend(self.next_publication(durable));
            }
            Mode::Follower(l) => {
                let msg = json!({"term": self.current_term, "leader": l.as_str()}).to_string();
                let rid = self.rid();
                out.push(
                    self.send(
                        from,
                        Envelope::request(JOIN, self.me.id.clone(), rid, vec![])
                            .error(self.me.id.clone(), &msg),
                    ),
                );
            }
        }
        out
    }

    // ---- being the manager -------------------------------------------------------

    fn become_leader(&mut self, durable: &mut Durable) -> Vec<Output> {
        self.mode = Mode::Leader;
        self.leader_hint = Some(self.me.id.clone());
        self.election_scheduled = false;
        self.election_attempt = 0;
        self.followers.clear();
        self.lagging.clear();
        let mut out = Vec::new();
        if self.notes {
            out.push(self.note(format!("leader term={}", self.current_term)));
        }
        // the first state of the term: the nodes that joined it
        let mut s = self.accepted.next();
        s.cluster_manager = Some(self.me.id.clone());
        s.cluster_uuid_committed = true;
        s.nodes = std::mem::take(&mut self.join_votes);
        s.nodes.insert(self.me.id.clone(), self.me.clone());
        for n in s.nodes.keys().filter(|n| **n != self.me.id) {
            self.followers.insert(n.clone(), FollowerHealth::default());
        }
        out.extend(self.place(&mut s));
        out.extend(self.publish(s, durable));
        out.push(Output::Timer {
            id: T_FOLLOWER_CHECK,
            after: self.timings.follower_check_interval,
        });
        if self.metadata.is_some() {
            out.push(Output::Timer { id: T_METADATA, after: self.metadata_poll });
        }
        out
    }

    /// Lay the manager's index metadata and shard placement over a state:
    /// what the data nodes reported, then the deciders and the balancer
    /// over the table as it was.
    fn place(&mut self, s: &mut ClusterState) -> Vec<Output> {
        let Some(src) = self.metadata.clone() else { return vec![] };
        let own = src.snapshot();
        let settings = src.cluster_settings();
        let cluster = super::allocation::ClusterSettings::from_value(&settings);
        let mut table = self.command_table.take().unwrap_or_else(|| s.routing.clone());
        // copies no longer in sync: failed, or the source of a finished move;
        // a primary made empty on request stands alone in its set
        let mut retired: BTreeMap<String, Vec<(u32, String)>> = BTreeMap::new();
        let mut reset: BTreeMap<String, Vec<(u32, String)>> = BTreeMap::new();
        for ev in std::mem::take(&mut self.shard_events) {
            match ev {
                ShardEvent::Started { index, shard, allocation_id } => {
                    if let Some(st) =
                        super::allocation::shard_started(&mut table, &index, shard, &allocation_id)
                    {
                        if let Some(r) = st.retired {
                            retired.entry(index.clone()).or_default().push((shard, r));
                        }
                        if st.forced_primary {
                            reset
                                .entry(index.clone())
                                .or_default()
                                .push((shard, allocation_id.clone()));
                        }
                    }
                }
                ShardEvent::Stale { index, shard, allocation_id } => {
                    retired.entry(index.clone()).or_default().push((shard, allocation_id));
                }
                ShardEvent::Failed { index, shard, allocation_id, message } => {
                    if super::allocation::shard_failed(
                        &mut table,
                        &index,
                        shard,
                        &allocation_id,
                        self.last_wall,
                        &message,
                    ) {
                        retired
                            .entry(index.clone())
                            .or_default()
                            .push((shard, allocation_id.clone()));
                    }
                }
            }
        }
        // what was in sync before this publication, by index
        let previous_in_sync: BTreeMap<String, BTreeMap<u32, Vec<String>>> =
            s.indices.iter().map(|(n, m)| (n.clone(), m.in_sync_allocations.clone())).collect();
        // an index's metadata belongs to the node holding its primary: this
        // node's store for the primaries here (and for indices not yet
        // published), the latest report for primaries elsewhere, and what
        // was published last for the rest; a deleted index goes to the graveyard
        let me = self.me.id.clone();
        let primary_of =
            |name: &str| -> Option<NodeId> { table.primary(name, 0).and_then(|p| p.node.clone()) };
        let mut base: BTreeMap<String, super::state::IndexMetadata> = s.indices.clone();
        for t in src.tombstones() {
            if let Some(name) = t.pointer("/index/index_name").and_then(|n| n.as_str()) {
                base.remove(name);
                self.reports.remove(name);
                s.graveyard.push(t.clone());
            }
        }
        if s.graveyard.len() > 500 {
            let drop = s.graveyard.len() - 500;
            s.graveyard.drain(..drop);
        }
        let buried = |name: &str, uuid: &str| {
            s.graveyard.iter().any(|t| {
                t.pointer("/index/index_uuid").and_then(|u| u.as_str()) == Some(uuid)
                    && t.pointer("/index/index_name").and_then(|n| n.as_str()) == Some(name)
            })
        };
        // a store's metadata knows nothing of which copies are in sync or
        // what term a primary is in: those come from what was published
        let carried = |name: &str, mut m: super::state::IndexMetadata| {
            if let Some(prev) = s.indices.get(name) {
                m.in_sync_allocations = prev.in_sync_allocations.clone();
                m.primary_terms = prev.primary_terms.clone();
            }
            m
        };
        let mut home: BTreeMap<String, NodeId> = BTreeMap::new();
        for (name, m) in &own {
            if buried(name, &m.uuid) {
                continue;
            }
            match primary_of(name) {
                Some(p) if p == me => {
                    base.insert(name.clone(), carried(name, m.clone()));
                }
                Some(_) => {}
                None => {
                    // not placed yet: a new index, whose primary is where its
                    // data is -- unless it was placed once and its primary is
                    // lost, which is not this node's to decide
                    let known = s.indices.contains_key(name);
                    base.insert(name.clone(), carried(name, m.clone()));
                    if !known {
                        home.insert(name.clone(), me.clone());
                    }
                }
            }
        }
        for (name, (node, m)) in &self.reports {
            if primary_of(name).as_ref() == Some(node) && !buried(name, &m.uuid) {
                base.insert(name.clone(), carried(name, m.clone()));
            }
        }
        let mut held = self.held.clone();
        held.insert(me.clone(), src.held());
        held.retain(|n, _| s.nodes.contains_key(n));
        let ctx = super::allocation::Context {
            nodes: &s.nodes,
            indices: &base,
            cluster: &cluster,
            primary_home: &home,
            held: &held,
            now: self.last_wall,
        };
        let (routing, changes) = super::allocation::reroute(&ctx, &table);
        let mut out = Vec::new();
        for (index, shard, _node) in &changes.promoted {
            *self.terms.entry((index.clone(), *shard)).or_insert(1) += 1;
        }
        if self.notes {
            for n in &changes.notes {
                out.push(self.note(format!("allocation: {n}")));
            }
        }
        if self.notes && !changes.is_empty() {
            out.push(self.note(format!(
                "allocation: assigned={} unassigned={} promoted={} relocating={}",
                changes.assigned.len(),
                changes.unassigned.len(),
                changes.promoted.len(),
                changes.relocating.len()
            )));
        }
        if let Some(at) = changes.next_delay_at {
            out.push(Output::Timer {
                id: T_ALLOCATE,
                after: at.saturating_sub(self.last_wall).max(1),
            });
        }
        let empty: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        s.indices = base
            .into_iter()
            .map(|(n, m)| {
                let prev = previous_in_sync.get(&n).unwrap_or(&empty);
                let ret = retired.get(&n).cloned().unwrap_or_default();
                let rs = reset.get(&n).cloned().unwrap_or_default();
                (n, super::metadata::with_terms(m, &routing, &self.terms, prev, &ret, &rs))
            })
            .collect();
        self.terms.retain(|(i, _), _| s.indices.contains_key(i));
        s.routing = routing;
        s.cluster_settings = settings;
        s.customs = src.customs();
        self.last_fingerprint = self.metadata_fingerprint(&own, &s.routing);
        out
    }

    /// What of this node's store the cluster would notice changing: the
    /// indices whose primary is here or that are not placed yet, the
    /// exclusions, the customs.
    fn metadata_fingerprint(
        &self,
        own: &BTreeMap<String, super::state::IndexMetadata>,
        routing: &super::state::RoutingTable,
    ) -> u64 {
        use std::hash::{Hash, Hasher};
        let mine: BTreeMap<String, super::state::IndexMetadata> = own
            .iter()
            .filter(|(n, _)| {
                routing
                    .primary(n, 0)
                    .and_then(|p| p.node.clone())
                    .map(|p| p == self.me.id)
                    .unwrap_or(true)
            })
            .map(|(n, m)| (n.clone(), m.clone()))
            .collect();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        super::metadata::fingerprint(&mine).hash(&mut h);
        if let Some(src) = &self.metadata {
            src.customs().to_string().hash(&mut h);
        }
        h.finish()
    }

    /// A follower holding primaries tells the manager about them when they
    /// change; anyone applies what the manager published to the copies here.
    fn report_metadata(&mut self) -> Vec<Output> {
        let Some(src) = self.metadata.clone() else { return vec![] };
        let Mode::Follower(leader) = self.mode.clone() else { return vec![] };
        let own = src.snapshot();
        let mut changed: Vec<Value> = Vec::new();
        for (name, m) in &own {
            let primary_here = self
                .committed
                .routing
                .primary(name, 0)
                .and_then(|p| p.node.clone())
                .map(|p| p == self.me.id)
                .unwrap_or(false);
            if !primary_here {
                continue;
            }
            let mut one = BTreeMap::new();
            one.insert(name.clone(), m.clone());
            let fp = super::metadata::fingerprint(&one);
            if self.reported_fp.get(name) != Some(&fp) {
                self.reported_fp.insert(name.clone(), fp);
                changed.push(serde_json::to_value(m).unwrap_or(Value::Null));
            }
        }
        let held = src.held();
        let held_changed = held != self.held_reported;
        if changed.is_empty() && !held_changed {
            return vec![];
        }
        self.held_reported = held.clone();
        vec![self.request(&leader, METADATA_REPORT, json!({"indices": changed, "held": held}))]
    }

    /// Published metadata reaches the store: the copies here of indices
    /// whose primary is elsewhere take their settings, mappings and
    /// aliases; the customs are taken whole; a local index the cluster
    /// buried or placed nowhere here is let go.
    fn apply_metadata(&mut self) -> Vec<Output> {
        let mut out = Vec::new();
        let Some(src) = self.metadata.clone() else { return out };
        let me = self.me.id.clone();
        let routing = &self.committed.routing;
        for (name, m) in &self.committed.indices {
            let primary_here = routing
                .primary(name, 0)
                .and_then(|p| p.node.clone())
                .map(|p| p == me)
                .unwrap_or(false);
            let copy_here = routing.on_node(&me).any(|c| c.index == *name);
            if copy_here && !primary_here {
                src.apply_index_metadata(m);
            }
        }
        {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            self.committed.customs.to_string().hash(&mut h);
            let fp = h.finish();
            if fp != self.customs_applied && self.mode != Mode::Leader {
                src.apply_customs(&self.committed.customs);
                self.customs_applied = fp;
            }
        }
        // what this node holds that the cluster does not place here: let go
        // only when the index is buried, or when the cluster has it active
        // elsewhere -- a copy of a primary that is lost may be the only data
        // left, and the manager takes it back once this node says it holds it
        for (name, m) in src.snapshot() {
            let buried = self.committed.graveyard.iter().any(|t| {
                t.pointer("/index/index_uuid").and_then(|u| u.as_str()) == Some(m.uuid.as_str())
            });
            let published = self.committed.indices.contains_key(&name);
            let copy_here = routing.on_node(&me).any(|c| c.index == name);
            let active_elsewhere = routing.shards_of(&name).any(|c| {
                c.node.as_ref() != Some(&me)
                    && c.primary
                    && matches!(
                        c.state,
                        super::state::ShardState::Started | super::state::ShardState::Relocating
                    )
            });
            if buried || (published && !copy_here && active_elsewhere && self.committed.version > 0)
            {
                if self.notes {
                    out.push(self.note(format!(
                        "dropped local copy of [{name}] ({})",
                        if buried { "buried" } else { "active elsewhere, not placed here" }
                    )));
                }
                src.drop_local(&name);
            }
        }
        out
    }

    /// The copies the committed state puts on this node: started here and
    /// reported to the manager; the ones no longer here let go.
    fn apply_shards(&mut self) -> Vec<Output> {
        let mut out = self.apply_metadata();
        let me = self.me.id.clone();
        // every copy here learns the allocation id the manager gave it
        if let Some(src) = self.metadata.clone() {
            for c in self.committed.routing.on_node(&me) {
                if let Some(a) = &c.allocation_id {
                    if c.state != super::state::ShardState::Unassigned {
                        src.note_allocation(&c.index, a);
                    }
                }
            }
        }
        let mine: Vec<super::state::ShardRouting> =
            self.committed.routing.on_node(&me).cloned().collect();
        // copies to start
        for copy in mine.iter().filter(|c| c.state == super::state::ShardState::Initializing) {
            let Some(aid) = copy.allocation_id.clone() else { continue };
            if self.reported.contains(&aid) {
                continue;
            }
            self.reported.insert(aid.clone());
            let meta = self.committed.indices.get(&copy.index).cloned();
            let result = match (&self.host, meta) {
                (Some(h), Some(m)) => h.start_shard(&m, copy),
                _ => Ok(true),
            };
            self.hosted.insert(aid.clone(), (copy.index.clone(), copy.shard));
            let ev = match result {
                // the host will say when the copy is ready
                Ok(false) => continue,
                Ok(true) => ShardEvent::Started {
                    index: copy.index.clone(),
                    shard: copy.shard,
                    allocation_id: aid,
                },
                Err(message) => ShardEvent::Failed {
                    index: copy.index.clone(),
                    shard: copy.shard,
                    allocation_id: aid,
                    message,
                },
            };
            out.extend(self.report(ev));
        }
        // copies that were here and are not any more
        let still: BTreeSet<String> = mine.iter().filter_map(|c| c.allocation_id.clone()).collect();
        let gone: Vec<(String, (String, u32))> = self
            .hosted
            .iter()
            .filter(|(aid, _)| !still.contains(*aid))
            .map(|(a, v)| (a.clone(), v.clone()))
            .collect();
        for (aid, (index, shard)) in gone {
            self.hosted.remove(&aid);
            self.reported.remove(&aid);
            // an index the manager still publishes and this node still holds
            // a copy of keeps its local index
            let holds_other = self.hosted.values().any(|(i, _)| *i == index);
            if !holds_other {
                if let Some(h) = &self.host {
                    h.remove_shard(&index, shard);
                }
            }
        }
        out
    }

    /// Tell the manager about a copy; the manager tells itself.
    fn report(&mut self, ev: ShardEvent) -> Vec<Output> {
        let (action, body) = match &ev {
            ShardEvent::Started { index, shard, allocation_id } => (
                SHARD_STARTED,
                json!({"index": index, "shard": shard, "allocation_id": allocation_id}),
            ),
            ShardEvent::Failed { index, shard, allocation_id, message } => (
                SHARD_FAILED,
                json!({"index": index, "shard": shard, "allocation_id": allocation_id, "message": message}),
            ),
            ShardEvent::Stale { index, shard, allocation_id } => (
                SHARD_STALE,
                json!({"index": index, "shard": shard, "allocation_id": allocation_id}),
            ),
        };
        match self.mode.clone() {
            Mode::Leader => {
                self.shard_events.push(ev);
                self.republish_wanted = true;
                vec![]
            }
            Mode::Follower(l) => vec![self.request(&l, action, body)],
            Mode::Candidate => vec![],
        }
    }

    /// The voting configuration the nodes call for, from the committed one:
    /// OpenSearch's reconfigurator.
    fn reconfigure(&self, s: &ClusterState) -> Vec<NodeId> {
        let current = &s.last_committed_config;
        let excluded = |id: &NodeId| {
            self.exclusions.iter().any(|(i, n)| {
                i == id.as_str() || s.nodes.get(id).map(|d| d.name == *n).unwrap_or(false)
            })
        };
        let live: BTreeSet<NodeId> = s
            .nodes
            .values()
            .filter(|n| n.is_cluster_manager_eligible() && !excluded(&n.id))
            .map(|n| n.id.clone())
            .collect();
        let non_retired_current: Vec<NodeId> =
            current.iter().filter(|id| !excluded(id)).cloned().collect();
        let min_enforced = if self.auto_shrink {
            if non_retired_current.len() < 3 { 1 } else { 3 }
        } else {
            non_retired_current.len()
        };
        let odd = |n: usize| if n % 2 == 0 { n.saturating_sub(1) } else { n };
        let target = odd(live.len()).max(min_enforced);
        let live_in = current.iter().filter(|id| live.contains(id)).cloned();
        let live_out = live.iter().filter(|id| !current.contains(id)).cloned();
        let dead_in = non_retired_current.iter().filter(|id| !live.contains(id)).cloned();
        let mut new: Vec<NodeId> = live_in.chain(live_out).chain(dead_in).take(target).collect();
        new.sort();
        // a configuration the live nodes cannot form a quorum of would stop the cluster
        if has_quorum(&new, &live) { new } else { current.clone() }
    }

    /// Publish a state: send it to every node in it; commit once a quorum
    /// of both configurations has accepted.
    fn publish(&mut self, mut state: ClusterState, durable: &mut Durable) -> Vec<Output> {
        state.term = self.current_term;
        state.cluster_manager = Some(self.me.id.clone());
        state.voting_config_exclusions =
            self.exclusions.iter().map(|(i, n)| json!({"node_id": i, "node_name": n})).collect();
        let mut out = Vec::new();
        let others: Vec<NodeId> =
            state.nodes.keys().filter(|n| **n != self.me.id).cloned().collect();
        let body = serde_json::to_vec(&state).unwrap_or_default();
        let rid = self.rid();
        for n in &others {
            out.push(
                self.send(n, Envelope::request(PUBLISH, self.me.id.clone(), rid, body.clone())),
            );
        }
        let mut acked = BTreeSet::new();
        acked.insert(self.me.id.clone());
        self.publish_seq += 1;
        self.publishing =
            Some(Publication { state: state.clone(), rid, seq: self.publish_seq, acked });
        self.accepted = state;
        self.persist_accepted(durable);
        out.push(Output::Timer { id: T_PUBLISH, after: self.timings.publish_timeout });
        if election_quorum(
            &self.accepted,
            &self.publishing.as_ref().map(|p| p.acked.clone()).unwrap_or_default(),
        ) {
            out.extend(self.commit_publication(durable));
        }
        out
    }

    fn commit_publication(&mut self, durable: &mut Durable) -> Vec<Output> {
        let mut out = Vec::new();
        let Some(p) = self.publishing.take() else { return out };
        let state = p.state;
        let body = serde_json::to_vec(&json!({"term": state.term, "version": state.version}))
            .unwrap_or_default();
        let rid = self.rid();
        // a node is told to commit once it has said it accepted: a commit
        // that overtook its publication would be refused
        for n in state.nodes.keys().filter(|n| **n != self.me.id && p.acked.contains(*n)) {
            out.push(
                self.send(n, Envelope::request(COMMIT, self.me.id.clone(), rid, body.clone())),
            );
        }
        self.lagging = state.nodes.keys().filter(|n| !p.acked.contains(*n)).cloned().collect();
        self.apply_committed(state.clone(), durable);
        out.extend(self.apply_shards());
        if self.notes {
            out.push(self.note(format!(
                "committed t{} v{} nodes={} config={}",
                state.term,
                state.version,
                state.nodes.len(),
                state.last_accepted_config.len()
            )));
        }
        out.extend(self.next_publication(durable));
        out
    }

    /// The next state, if anything asks for one: joins and departures that
    /// waited, metadata that moved, a voting configuration to improve.
    fn next_publication(&mut self, durable: &mut Durable) -> Vec<Output> {
        if self.mode != Mode::Leader || self.publishing.is_some() {
            return vec![];
        }
        let joins = std::mem::take(&mut self.waiting_joins);
        let removals = std::mem::take(&mut self.pending_removals);
        let mut s = self.committed.next();
        let mut changed = !joins.is_empty()
            || self.republish_wanted
            || !self.shard_events.is_empty()
            || self.command_table.is_some();
        for (id, j) in joins {
            s.nodes.insert(id, j);
        }
        for r in &removals {
            if s.nodes.remove(r).is_some() {
                changed = true;
            }
            self.followers.remove(r);
        }
        let config = self.reconfigure(&s);
        if config != s.last_accepted_config {
            s.last_accepted_config = config;
            changed = true;
        }
        if !changed {
            return vec![];
        }
        self.republish_wanted = false;
        let mut out = self.place(&mut s);
        for n in s.nodes.keys().filter(|n| **n != self.me.id) {
            self.followers.entry(n.clone()).or_default();
        }
        out.extend(self.publish(s, durable));
        out
    }

    fn apply_committed(&mut self, mut state: ClusterState, durable: &mut Durable) {
        if !fresher(state.term, state.version, self.committed.term, self.committed.version) {
            return;
        }
        // committing a state commits the configuration it carried
        state.last_committed_config = state.last_accepted_config.clone();
        if fresher(state.term, state.version, self.accepted.term, self.accepted.version) {
            self.accepted = state.clone();
            self.persist_accepted(durable);
        }
        self.committed = state;
        self.persist_committed(durable);
    }
}

impl NodeLogic for Coordinator {
    fn handle(&mut self, input: Input, clock: &dyn Clock, durable: &mut Durable) -> Vec<Output> {
        self.last_wall = clock.wall();
        match input {
            Input::Start => {
                // what was on disk survives a restart
                if let Some(bytes) = durable.entries.get(D_COMMITTED) {
                    if let Ok(s) = serde_json::from_slice::<ClusterState>(bytes) {
                        self.committed = s;
                    }
                }
                self.accepted = self.committed.clone();
                if let Some(bytes) = durable.entries.get(D_ACCEPTED) {
                    if let Ok(s) = serde_json::from_slice::<ClusterState>(bytes) {
                        if fresher(s.term, s.version, self.accepted.term, self.accepted.version) {
                            self.accepted = s;
                        }
                    }
                }
                if let Some(bytes) = durable.entries.get(D_TERM) {
                    self.current_term = String::from_utf8_lossy(bytes).trim().parse().unwrap_or(0);
                }
                self.current_term = self.current_term.max(self.accepted.term);
                self.max_term_seen = self.current_term;
                let known: Vec<DiscoveryNode> = self.accepted.nodes.values().cloned().collect();
                let mut out = Vec::new();
                for n in known {
                    out.extend(self.learn(n));
                }
                out.extend(self.bootstrap(durable));
                out.extend(self.become_candidate());
                if self.metadata.is_some() {
                    out.push(Output::Timer { id: T_METADATA, after: self.metadata_poll });
                }
                out
            }
            Input::Timer(T_DISCOVER) => {
                // a manager the peers named last round has to be named again
                // this round, or it is not one this node keeps waiting for
                self.leader_hint = None;
                let mut out = self.bootstrap(durable);
                out.extend(self.discover());
                out
            }
            Input::Timer(T_ELECTION) => {
                self.election_scheduled = false;
                if self.mode != Mode::Candidate || !self.eligible() {
                    return vec![];
                }
                let mut out = Vec::new();
                if let Some(l) = self.leader_hint.clone() {
                    out.push(self.join(&l));
                }
                if !self.accepted.last_accepted_config.is_empty() {
                    out.extend(self.prevote());
                    if self.prevote_quorum() {
                        out.extend(self.start_election(durable));
                    }
                }
                if self.mode == Mode::Candidate {
                    out.push(self.schedule_election());
                }
                out
            }
            Input::Timer(T_PUBLISH) => {
                let timed_out =
                    self.publishing.as_ref().map(|p| p.seq == self.publish_seq).unwrap_or(false);
                if !timed_out || self.mode != Mode::Leader {
                    return vec![];
                }
                // no quorum in time: this node is not the manager of anything
                let mut out = Vec::new();
                if self.notes {
                    let v = self.publishing.as_ref().map(|p| p.state.version).unwrap_or(0);
                    out.push(self.note(format!("publication v{v} timed out: stepping down")));
                }
                out.extend(self.become_candidate());
                out
            }
            Input::Timer(T_FOLLOWER_CHECK) => {
                if self.mode != Mode::Leader {
                    return vec![];
                }
                let mut out = Vec::new();
                // unanswered checks count as misses
                let missed: Vec<NodeId> = self.pending_checks.values().cloned().collect();
                self.pending_checks.clear();
                for n in missed {
                    let h = self.followers.entry(n.clone()).or_default();
                    h.misses += 1;
                    if h.misses > self.timings.follower_check_retries {
                        if self.notes {
                            out.push(self.note(format!("removed {n}")));
                        }
                        self.pending_removals.insert(n);
                    }
                }
                out.extend(self.next_publication(durable));
                // the nodes of the latest publication, committed or on its way
                let followers: Vec<NodeId> =
                    self.accepted.nodes.keys().filter(|n| **n != self.me.id).cloned().collect();
                let body =
                    serde_json::to_vec(&json!({"term": self.current_term})).unwrap_or_default();
                for n in followers {
                    if self.pending_removals.contains(&n) {
                        continue;
                    }
                    let rid = self.rid();
                    self.pending_checks.insert(rid, n.clone());
                    out.push(self.send(
                        &n,
                        Envelope::request(FOLLOWER_CHECK, self.me.id.clone(), rid, body.clone()),
                    ));
                }
                out.push(Output::Timer {
                    id: T_FOLLOWER_CHECK,
                    after: self.timings.follower_check_interval,
                });
                out
            }
            Input::Timer(T_METADATA) => {
                let mut out = Vec::new();
                if self.mode == Mode::Leader {
                    if let Some(m) = self.metadata.as_ref() {
                        let own = m.snapshot();
                        let routing = self.committed.routing.clone();
                        if self.metadata_fingerprint(&own, &routing) != self.last_fingerprint {
                            self.republish_wanted = true;
                        }
                        if m.has_tombstones() {
                            // the publication takes them into the graveyard
                            self.republish_wanted = true;
                        }
                        let ex = m.voting_exclusions();
                        if ex != self.exclusions {
                            self.exclusions = ex;
                            self.republish_wanted = true;
                        }
                    }
                    if self.republish_wanted && self.notes && self.publishing.is_none() {
                        out.push(self.note("metadata changed".into()));
                    }
                    out.extend(self.next_publication(durable));
                } else {
                    out.extend(self.report_metadata());
                }
                if self.metadata.is_some() {
                    out.push(Output::Timer { id: T_METADATA, after: self.metadata_poll });
                }
                out
            }
            Input::Timer(T_ALLOCATE) => {
                if self.mode != Mode::Leader {
                    return vec![];
                }
                self.republish_wanted = true;
                self.next_publication(durable)
            }
            Input::Timer(T_LEADER_CHECK) => {
                let Mode::Follower(leader) = self.mode.clone() else { return vec![] };
                let mut out = Vec::new();
                if self.leader_check_outstanding.is_some() {
                    self.leader_misses += 1;
                    if self.leader_misses > self.timings.leader_check_retries {
                        if self.notes {
                            out.push(self.note(format!("lost manager {leader}")));
                        }
                        out.extend(self.become_candidate());
                        return out;
                    }
                }
                let rid = self.rid();
                self.leader_check_outstanding = Some(rid);
                out.push(self.send(
                    &leader,
                    Envelope::request(LEADER_CHECK, self.me.id.clone(), rid, vec![]),
                ));
                out.push(Output::Timer {
                    id: T_LEADER_CHECK,
                    after: self.timings.leader_check_interval,
                });
                out
            }
            Input::Timer(_) => vec![],
            Input::Peer(node) => {
                let mut out = self.learn(node);
                out.extend(self.bootstrap(durable));
                out
            }
            Input::ShardDone { allocation_id, result } => {
                // the copy the host was building: reported now, if it is still ours
                let Some((index, shard)) = self.hosted.get(&allocation_id).cloned() else {
                    return vec![];
                };
                let mut out = Vec::new();
                let ev =
                    match result {
                        Ok(()) => ShardEvent::Started { index, shard, allocation_id },
                        Err(message) => {
                            if self.notes {
                                out.push(self.note(format!(
                                    "shard [{index}][{shard}] failed here: {message}"
                                )));
                            }
                            ShardEvent::Failed { index, shard, allocation_id, message }
                        }
                    };
                out.extend(self.report(ev));
                out.extend(self.next_publication(durable));
                out
            }
            Input::Message(e) => self.on_message(e, durable),
        }
    }
}

fn out_note_holder(_out: &mut Vec<Output>, _what: &str) {}

fn term_in(body: &[u8]) -> Option<u64> {
    serde_json::from_slice::<Value>(body).ok()?.get("term")?.as_u64()
}

impl Coordinator {
    fn on_message(&mut self, e: Envelope, durable: &mut Durable) -> Vec<Output> {
        let from = e.from.clone();
        match (e.action.as_str(), e.kind) {
            (PEERS, Kind::Request) => {
                let mut out = Vec::new();
                if let Some(n) = serde_json::from_slice::<Value>(&e.body).ok().and_then(|v| {
                    serde_json::from_value::<DiscoveryNode>(v.get("node")?.clone()).ok()
                }) {
                    out.extend(self.learn(n));
                }
                let leader = match &self.mode {
                    Mode::Leader => Some(self.me.id.as_str().to_string()),
                    Mode::Follower(l) => Some(l.as_str().to_string()),
                    Mode::Candidate => self.leader_hint.as_ref().map(|l| l.as_str().to_string()),
                };
                let mut peers: Vec<&DiscoveryNode> = self.peers.values().collect();
                peers.push(&self.me);
                let body = serde_json::to_vec(&json!({"peers": peers, "leader": leader}))
                    .unwrap_or_default();
                out.push(self.send(&from, e.response(self.me.id.clone(), body)));
                out
            }
            (PEERS, Kind::Response) => {
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let mut out = Vec::new();
                if let Some(ps) = v.get("peers").and_then(|p| p.as_array()) {
                    for p in ps {
                        if let Ok(n) = serde_json::from_value::<DiscoveryNode>(p.clone()) {
                            out.extend(self.learn(n));
                        }
                    }
                }
                if let Some(l) = v.get("leader").and_then(|l| l.as_str()) {
                    let l = NodeId(l.to_string());
                    if l != self.me.id
                        && self.mode == Mode::Candidate
                        && self.leader_hint.as_ref() != Some(&l)
                    {
                        self.leader_hint = Some(l.clone());
                        out.push(self.join(&l));
                    }
                }
                out
            }
            (PRE_VOTE, Kind::Request) => {
                let term = term_in(&e.body).unwrap_or(0);
                self.max_term_seen = self.max_term_seen.max(term);
                // a node with a manager does not encourage another
                let leader = match &self.mode {
                    Mode::Leader => Some(self.me.id.clone()),
                    Mode::Follower(l) => Some(l.clone()),
                    Mode::Candidate => None,
                };
                if let Some(l) = leader.filter(|l| *l != from) {
                    let msg = json!({"term": self.current_term, "leader": l.as_str()}).to_string();
                    return vec![self.send(&from, e.error(self.me.id.clone(), &msg))];
                }
                let body = serde_json::to_vec(&json!({
                    "term": self.current_term,
                    "last_accepted_term": self.accepted.term,
                    "last_accepted_version": self.accepted.version,
                }))
                .unwrap_or_default();
                vec![self.send(&from, e.response(self.me.id.clone(), body))]
            }
            (PRE_VOTE, Kind::Response) => {
                if self.mode != Mode::Candidate || e.request_id != self.prevote_rid {
                    return vec![];
                }
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let term = v.get("term").and_then(|t| t.as_u64()).unwrap_or(0);
                let la_term = v.get("last_accepted_term").and_then(|t| t.as_u64()).unwrap_or(0);
                let la_version =
                    v.get("last_accepted_version").and_then(|t| t.as_u64()).unwrap_or(0);
                self.max_term_seen = self.max_term_seen.max(term);
                if fresher(la_term, la_version, self.accepted.term, self.accepted.version) {
                    // it has accepted more than this node: this node must not lead
                    return vec![];
                }
                self.prevotes.insert(from, (term, la_term, la_version));
                if self.prevote_quorum() {
                    self.prevotes.clear();
                    return self.start_election(durable);
                }
                vec![]
            }
            (PRE_VOTE, Kind::Error) => {
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let mut out = Vec::new();
                if let Some(t) = v.get("term").and_then(|t| t.as_u64()) {
                    self.max_term_seen = self.max_term_seen.max(t);
                }
                if let Some(l) = v.get("leader").and_then(|l| l.as_str()) {
                    let l = NodeId(l.to_string());
                    if l != self.me.id && self.mode == Mode::Candidate {
                        self.leader_hint = Some(l.clone());
                        out.push(self.join(&l));
                    }
                }
                out
            }
            (START_JOIN, Kind::Request) => {
                let term = term_in(&e.body).unwrap_or(0);
                let mut out = vec![self.send(&from, e.response(self.me.id.clone(), vec![]))];
                out.extend(self.on_start_join(term, &from, durable));
                out
            }
            (JOIN, Kind::Request) => {
                let body: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                self.on_join(body, &from, durable)
            }
            (JOIN, Kind::Error) => {
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let mut out = Vec::new();
                if let Some(t) = v.get("term").and_then(|t| t.as_u64()) {
                    out.extend(self.saw_term(t, durable));
                }
                match v.get("leader").and_then(|l| l.as_str()) {
                    Some(l) if l != self.me.id.as_str() => {
                        let l = NodeId(l.to_string());
                        if self.mode == Mode::Candidate && self.leader_hint.as_ref() != Some(&l) {
                            self.leader_hint = Some(l.clone());
                            out.push(self.join(&l));
                        }
                    }
                    _ => {
                        if self.leader_hint.as_ref() == Some(&from) {
                            self.leader_hint = None;
                        }
                    }
                }
                out
            }
            (JOIN, Kind::Response) => {
                if let Ok(s) = serde_json::from_slice::<ClusterState>(&e.body) {
                    if s.term == self.current_term {
                        self.apply_committed(s, durable);
                        let was = self.mode.clone();
                        self.mode = Mode::Follower(from.clone());
                        self.leader_hint = Some(from);
                        let mut out = self.apply_shards();
                        if was == Mode::Candidate {
                            self.leader_misses = 0;
                            self.leader_check_outstanding = None;
                            out.push(Output::Timer {
                                id: T_LEADER_CHECK,
                                after: self.timings.leader_check_interval,
                            });
                        }
                        return out;
                    }
                }
                vec![]
            }
            (PUBLISH, Kind::Request) => {
                let Ok(s) = serde_json::from_slice::<ClusterState>(&e.body) else { return vec![] };
                let refuse = |reason: &str, me: &Coordinator| {
                    json!({"term": me.current_term, "reason": reason}).to_string()
                };
                if s.term != self.current_term {
                    let msg = refuse("term", self);
                    return vec![self.send(&from, e.error(self.me.id.clone(), &msg))];
                }
                if s.term == self.accepted.term && s.version <= self.accepted.version {
                    let msg = refuse("stale", self);
                    return vec![self.send(&from, e.error(self.me.id.clone(), &msg))];
                }
                if self.committed.cluster_uuid_committed
                    && s.cluster_uuid != self.committed.cluster_uuid
                {
                    let msg = refuse("cluster_uuid", self);
                    return vec![self.send(&from, e.error(self.me.id.clone(), &msg))];
                }
                let leader = s.cluster_manager.clone().unwrap_or_else(|| from.clone());
                let body = serde_json::to_vec(&json!({"term": s.term, "version": s.version}))
                    .unwrap_or_default();
                self.accepted = s;
                self.persist_accepted(durable);
                let was_following = self.mode == Mode::Follower(leader.clone());
                self.mode = Mode::Follower(leader.clone());
                self.leader_hint = Some(leader);
                self.election_scheduled = false;
                let mut out = vec![self.send(&from, e.response(self.me.id.clone(), body))];
                if !was_following {
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
                let quorum = match self.publishing.as_mut() {
                    Some(p) if p.rid == e.request_id => {
                        p.acked.insert(from.clone());
                        election_quorum(&p.state, &p.acked)
                    }
                    _ => false,
                };
                if quorum {
                    return self.commit_publication(durable);
                }
                // accepted after the commit went out: its commit goes now
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let term = v.get("term").and_then(|t| t.as_u64()).unwrap_or(0);
                let version = v.get("version").and_then(|t| t.as_u64()).unwrap_or(0);
                if self.publishing.is_none()
                    && term == self.committed.term
                    && version == self.committed.version
                    && self.committed.nodes.contains_key(&from)
                {
                    self.lagging.remove(&from);
                    return vec![self.request(
                        &from,
                        COMMIT,
                        json!({"term": term, "version": version}),
                    )];
                }
                vec![]
            }
            (PUBLISH, Kind::Error) => {
                let mut out = Vec::new();
                if let Some(t) = term_in(&e.body) {
                    out.extend(self.saw_term(t, durable));
                }
                self.lagging.insert(from);
                out
            }
            (COMMIT, Kind::Request) => {
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let term = v.get("term").and_then(|t| t.as_u64()).unwrap_or(0);
                let version = v.get("version").and_then(|t| t.as_u64()).unwrap_or(0);
                if term == self.current_term
                    && self.accepted.term == term
                    && self.accepted.version == version
                {
                    let a = self.accepted.clone();
                    self.apply_committed(a, durable);
                    let mut out = vec![self.send(&from, e.response(self.me.id.clone(), vec![]))];
                    out.extend(self.apply_shards());
                    if self.notes {
                        out.push(self.note(format!(
                            "committed t{term} v{version} nodes={} config={}",
                            self.committed.nodes.len(),
                            self.committed.last_accepted_config.len()
                        )));
                    }
                    return out;
                }
                vec![]
            }
            (FOLLOWER_CHECK, Kind::Request) => {
                let term = term_in(&e.body).unwrap_or(0);
                let mut out = Vec::new();
                if term < self.current_term {
                    let msg = json!({"term": self.current_term}).to_string();
                    out.push(self.send(&from, e.error(self.me.id.clone(), &msg)));
                    return out;
                }
                out.extend(self.saw_term(term, durable));
                match self.mode.clone() {
                    Mode::Follower(l) if l == from => {
                        out.push(self.send(&from, e.response(self.me.id.clone(), vec![])));
                    }
                    Mode::Candidate => {
                        // a manager in this term checking on this node: follow it
                        self.mode = Mode::Follower(from.clone());
                        self.leader_hint = Some(from.clone());
                        self.election_scheduled = false;
                        self.leader_misses = 0;
                        self.leader_check_outstanding = None;
                        out.push(self.send(&from, e.response(self.me.id.clone(), vec![])));
                        out.push(Output::Timer {
                            id: T_LEADER_CHECK,
                            after: self.timings.leader_check_interval,
                        });
                    }
                    _ => {
                        let msg = json!({"term": self.current_term, "reason": "not my manager"})
                            .to_string();
                        out.push(self.send(&from, e.error(self.me.id.clone(), &msg)));
                    }
                }
                out
            }
            (FOLLOWER_CHECK, Kind::Response) => {
                if let Some(n) = self.pending_checks.remove(&e.request_id) {
                    if let Some(h) = self.followers.get_mut(&n) {
                        h.misses = 0;
                    }
                }
                vec![]
            }
            (FOLLOWER_CHECK, Kind::Error) => match term_in(&e.body) {
                Some(t) => self.saw_term(t, durable),
                None => vec![],
            },
            (LEADER_CHECK, Kind::Request) => {
                if self.mode == Mode::Leader && self.committed.nodes.contains_key(&from) {
                    vec![self.send(&from, e.response(self.me.id.clone(), vec![]))]
                } else {
                    let msg =
                        json!({"term": self.current_term, "reason": "not the manager"}).to_string();
                    vec![self.send(&from, e.error(self.me.id.clone(), &msg))]
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
                // the node this one follows says it is not the manager: look again
                let mut out = Vec::new();
                if let Some(t) = term_in(&e.body) {
                    out.extend(self.saw_term(t, durable));
                }
                if let Mode::Follower(l) = self.mode.clone() {
                    if l == from {
                        out.extend(self.become_candidate());
                    }
                }
                out
            }
            (SHARD_STARTED, Kind::Request)
            | (SHARD_FAILED, Kind::Request)
            | (SHARD_STALE, Kind::Request) => {
                if self.mode != Mode::Leader {
                    let who = match &self.mode {
                        Mode::Follower(l) => Some(l.as_str().to_string()),
                        _ => None,
                    };
                    let msg = json!({"term": self.current_term, "leader": who, "reason": "not the manager"}).to_string();
                    return vec![self.send(&from, e.error(self.me.id.clone(), &msg))];
                }
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let index = v.get("index").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let shard = v.get("shard").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                let allocation_id =
                    v.get("allocation_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let ev = if e.action == SHARD_STARTED {
                    ShardEvent::Started { index, shard, allocation_id }
                } else if e.action == SHARD_STALE {
                    ShardEvent::Stale { index, shard, allocation_id }
                } else {
                    let message =
                        v.get("message").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    ShardEvent::Failed { index, shard, allocation_id, message }
                };
                if self.notes {
                    let what = match &ev {
                        ShardEvent::Started { index, shard, .. } => {
                            format!("shard started [{index}][{shard}] on {from}")
                        }
                        ShardEvent::Failed { index, shard, message, .. } => {
                            format!("shard failed [{index}][{shard}] on {from}: {message}")
                        }
                        ShardEvent::Stale { index, shard, .. } => {
                            format!("shard stale [{index}][{shard}]")
                        }
                    };
                    out_note_holder(&mut Vec::new(), &what);
                }
                self.shard_events.push(ev);
                self.republish_wanted = true;
                let mut out = vec![self.send(&from, e.response(self.me.id.clone(), vec![]))];
                out.extend(self.next_publication(durable));
                out
            }
            (METADATA_REPORT, Kind::Request) => {
                if self.mode != Mode::Leader {
                    let msg =
                        json!({"term": self.current_term, "reason": "not the manager"}).to_string();
                    return vec![self.send(&from, e.error(self.me.id.clone(), &msg))];
                }
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                if let Some(list) = v.get("indices").and_then(|i| i.as_array()) {
                    for m in list {
                        if let Ok(meta) =
                            serde_json::from_value::<super::state::IndexMetadata>(m.clone())
                        {
                            self.reports.insert(meta.name.clone(), (from.clone(), meta));
                        }
                    }
                }
                if let Some(h) = v.get("held").and_then(|h| {
                    serde_json::from_value::<Vec<(String, String, String)>>(h.clone()).ok()
                }) {
                    self.held.insert(from.clone(), h);
                }
                self.republish_wanted = true;
                let mut out = vec![self.send(&from, e.response(self.me.id.clone(), vec![]))];
                out.extend(self.next_publication(durable));
                out
            }
            (REROUTE, Kind::Request) => {
                if self.mode != Mode::Leader {
                    let who = match &self.mode {
                        Mode::Follower(l) => Some(l.as_str().to_string()),
                        _ => None,
                    };
                    let msg = json!({"term": self.current_term, "leader": who, "reason": "not the manager"}).to_string();
                    return vec![self.send(&from, e.error(self.me.id.clone(), &msg))];
                }
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                let commands: Vec<Value> =
                    v.get("commands").and_then(|c| c.as_array()).cloned().unwrap_or_default();
                let dry_run = v.get("dry_run").and_then(|d| d.as_bool()).unwrap_or(false);
                let explain = v.get("explain").and_then(|d| d.as_bool()).unwrap_or(false);
                let retry = v.get("retry_failed").and_then(|d| d.as_bool()).unwrap_or(false);
                let Some(src) = self.metadata.clone() else {
                    return vec![
                        self.send(&from, e.error(self.me.id.clone(), "no metadata source")),
                    ];
                };
                let snapshot = src.snapshot();
                let settings = src.cluster_settings();
                let cluster = super::allocation::ClusterSettings::from_value(&settings);
                let home: BTreeMap<String, NodeId> =
                    snapshot.keys().map(|n| (n.clone(), self.me.id.clone())).collect();
                let nodes = self.committed.nodes.clone();
                let held = self.held.clone();
                let ctx = super::allocation::Context {
                    nodes: &nodes,
                    indices: &snapshot,
                    cluster: &cluster,
                    primary_home: &home,
                    held: &held,
                    now: self.last_wall,
                };
                let mut table = self.committed.routing.clone();
                if retry {
                    super::allocation::retry_failed(&mut table);
                }
                let (table, explanations) =
                    match super::allocation::apply_commands(&ctx, &table, &commands, explain) {
                        Ok(x) => x,
                        Err(msg) => {
                            let body = json!({"reason": msg}).to_string();
                            return vec![self.send(&from, e.error(self.me.id.clone(), &body))];
                        }
                    };
                // the state as it would be: the commands, then the deciders over them
                let (after, _) = super::allocation::reroute(&ctx, &table);
                let mut preview = self.committed.clone();
                preview.routing = after;
                let body = serde_json::to_vec(&json!({
                    "state": preview,
                    "explanations": explanations,
                }))
                .unwrap_or_default();
                let mut out = vec![self.send(&from, e.response(self.me.id.clone(), body))];
                if !dry_run {
                    self.command_table = Some(table);
                    self.republish_wanted = true;
                    out.extend(self.next_publication(durable));
                }
                out
            }
            (LEAVE, Kind::Request) => {
                if self.mode == Mode::Leader {
                    self.pending_removals.insert(from.clone());
                    let mut out = vec![self.send(&from, e.response(self.me.id.clone(), vec![]))];
                    out.extend(self.next_publication(durable));
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

    /// A cluster of named nodes, each seeded with all the others, with the
    /// named initial cluster manager nodes.
    fn cluster(seed: u64, names: &[&str], initial: &[&str]) -> (Sim, Vec<NodeId>) {
        let mut sim = Sim::new(seed);
        let ids: Vec<NodeId> = names.iter().map(|n| NodeId((*n).into())).collect();
        for n in names {
            let me = node(n);
            let initial: Vec<String> = initial.iter().map(|s| s.to_string()).collect();
            let seeds: Vec<NodeId> = ids.iter().filter(|i| i.as_str() != *n).cloned().collect();
            sim.add_node(
                me.id.clone(),
                Box::new(move |_| {
                    Box::new(Coordinator::new(
                        node(&me.name),
                        "c",
                        "cluster-uuid",
                        initial.clone(),
                        seeds.clone(),
                    ))
                }),
            );
        }
        (sim, ids)
    }

    fn committed_of(sim: &Sim, id: &NodeId) -> Option<ClusterState> {
        let bytes = sim.durable(id)?.entries.get(D_COMMITTED)?;
        serde_json::from_slice(bytes).ok()
    }

    /// The committed state of every node that is up: (name, term, version, nodes).
    fn versions(sim: &Sim, ids: &[NodeId]) -> Vec<(String, u64, u64, usize)> {
        ids.iter()
            .filter(|id| sim.is_up(id))
            .filter_map(|id| {
                let s = committed_of(sim, id)?;
                Some((id.0.clone(), s.term, s.version, s.nodes.len()))
            })
            .collect()
    }

    /// The leaders elected so far, by term, from the notes.
    fn leaders_by_term(sim: &Sim) -> BTreeMap<u64, BTreeSet<NodeId>> {
        let mut m: BTreeMap<u64, BTreeSet<NodeId>> = BTreeMap::new();
        for (_, n, t) in &sim.notes {
            if let Some(rest) = t.strip_prefix("leader term=") {
                let term: u64 = rest.parse().unwrap();
                m.entry(term).or_default().insert(n.clone());
            }
        }
        m
    }

    /// The invariants every test holds the cluster to.
    fn check_invariants(sim: &Sim, ids: &[NodeId]) {
        for (term, leaders) in leaders_by_term(sim) {
            assert_eq!(leaders.len(), 1, "term {term} had leaders {leaders:?}");
        }
        // two nodes that committed the same term and version committed the same state
        let mut by_key: BTreeMap<(u64, u64), BTreeSet<Vec<u8>>> = BTreeMap::new();
        for id in ids {
            if let Some(d) = sim.durable(id) {
                if let Some(bytes) = d.entries.get(D_COMMITTED) {
                    let s: ClusterState = serde_json::from_slice(bytes).unwrap();
                    // version 0 is the empty state every node starts from, not a commit
                    if s.version > 0 {
                        by_key.entry((s.term, s.version)).or_default().insert(bytes.clone());
                    }
                }
            }
        }
        for (k, states) in by_key {
            assert_eq!(states.len(), 1, "different states committed at {k:?}");
        }
    }

    /// The cluster has settled: one manager, and every node that is up has
    /// committed the same state, which holds every node that is up.
    fn assert_settled(sim: &Sim, ids: &[NodeId]) {
        let v = versions(sim, ids);
        let up = ids.iter().filter(|i| sim.is_up(i)).count();
        assert!(v.iter().all(|(_, _, _, n)| *n == up), "{v:?}");
        let keys: BTreeSet<(u64, u64)> = v.iter().map(|(_, t, ver, _)| (*t, *ver)).collect();
        assert_eq!(keys.len(), 1, "{v:?}");
        check_invariants(sim, ids);
    }

    #[test]
    fn three_nodes_elect_a_manager_and_agree() {
        let (mut sim, ids) = cluster(1, &["a", "b", "c"], &["a", "b", "c"]);
        sim.run_until(10_000);
        assert_settled(&sim, &ids);
        let s = committed_of(&sim, &ids[0]).unwrap();
        assert_eq!(s.last_committed_config.len(), 3);
        assert!(s.cluster_manager.is_some());
        assert_eq!(leaders_by_term(&sim).len(), 1, "{:?}", sim.notes);
    }

    #[test]
    fn one_named_manager_and_two_that_join_it() {
        let (mut sim, ids) = cluster(1, &["m", "a", "b"], &["m"]);
        sim.run_until(10_000);
        assert_settled(&sim, &ids);
        let s = committed_of(&sim, &ids[0]).unwrap();
        assert_eq!(s.cluster_manager, Some(NodeId("m".into())));
        // three eligible nodes: the configuration grew to all of them
        assert_eq!(s.last_committed_config.len(), 3, "{:?}", s.last_committed_config);
    }

    #[test]
    fn the_manager_dies_and_another_is_elected() {
        let (mut sim, ids) = cluster(2, &["a", "b", "c"], &["a", "b", "c"]);
        sim.run_until(10_000);
        assert_settled(&sim, &ids);
        let first = committed_of(&sim, &ids[0]).unwrap();
        let leader = first.cluster_manager.clone().unwrap();
        sim.crash(&leader);
        sim.run_until(40_000);
        assert_settled(&sim, &ids);
        let up: Vec<NodeId> = ids.iter().filter(|i| **i != leader).cloned().collect();
        let second = committed_of(&sim, &up[0]).unwrap();
        assert!(second.term > first.term, "{} vs {}", second.term, first.term);
        assert_ne!(second.cluster_manager, Some(leader.clone()));
        assert_eq!(second.nodes.len(), 2);
        // the dead node stays in the configuration: it cannot shrink below three
        assert_eq!(second.last_committed_config.len(), 3);
        // the old manager comes back as a follower of the new one
        sim.restart(&leader);
        sim.run_until(70_000);
        assert_settled(&sim, &ids);
        let third = committed_of(&sim, &leader).unwrap();
        assert_eq!(third.cluster_manager, second.cluster_manager);
        assert_eq!(third.nodes.len(), 3);
    }

    #[test]
    fn a_manager_cut_off_from_the_majority_steps_down() {
        let (mut sim, ids) = cluster(3, &["a", "b", "c"], &["a", "b", "c"]);
        sim.run_until(10_000);
        assert_settled(&sim, &ids);
        let old = committed_of(&sim, &ids[0]).unwrap();
        let leader = old.cluster_manager.clone().unwrap();
        let others: Vec<NodeId> = ids.iter().filter(|i| **i != leader).cloned().collect();
        sim.partition(&[leader.clone()], &others);
        sim.run_until(70_000);
        // the majority elected a manager of its own
        let new = committed_of(&sim, &others[0]).unwrap();
        assert!(new.term > old.term, "{:?}", sim.notes);
        assert!(others.contains(new.cluster_manager.as_ref().unwrap()));
        assert_eq!(committed_of(&sim, &others[1]).unwrap().version, new.version);
        // the cut-off manager could commit nothing and gave up
        assert!(
            sim.notes.iter().any(|(_, n, t)| *n == leader && t.contains("stepping down")),
            "{:?}",
            sim.notes
        );
        let stale = committed_of(&sim, &leader).unwrap();
        assert_eq!(stale.version, old.version, "the minority committed on its own");
        check_invariants(&sim, &ids);
        sim.heal();
        sim.run_until(110_000);
        assert_settled(&sim, &ids);
        let after = committed_of(&sim, &leader).unwrap();
        assert_eq!(after.cluster_manager, new.cluster_manager);
    }

    #[test]
    fn the_voting_configuration_follows_the_nodes() {
        let names = ["a", "b", "c", "d", "e"];
        let (mut sim, ids) = cluster(4, &names, &names);
        sim.run_until(15_000);
        assert_settled(&sim, &ids);
        let s = committed_of(&sim, &ids[0]).unwrap();
        assert_eq!(s.last_committed_config.len(), 5);
        let leader = s.cluster_manager.clone().unwrap();
        let victims: Vec<NodeId> = ids.iter().filter(|i| **i != leader).take(2).cloned().collect();
        for v in &victims {
            sim.crash(v);
        }
        sim.run_until(45_000);
        assert_settled(&sim, &ids);
        let s = committed_of(&sim, &leader).unwrap();
        assert_eq!(s.nodes.len(), 3);
        assert_eq!(s.last_committed_config.len(), 3, "{:?}", s.last_committed_config);
        assert!(s.last_committed_config.iter().all(|n| s.nodes.contains_key(n)));
        // one more goes: two live of three, still a quorum, and no shrinking below three
        let third = ids.iter().find(|i| **i != leader && !victims.contains(i)).unwrap().clone();
        sim.crash(&third);
        sim.run_until(75_000);
        let s = committed_of(&sim, &leader).unwrap();
        assert_eq!(s.nodes.len(), 2);
        assert_eq!(s.last_committed_config.len(), 3);
        assert!(s.last_committed_config.contains(&third));
        check_invariants(&sim, &ids);
    }

    /// A metadata source whose exclusions a test can set.
    struct Excluding(parking_lot::Mutex<Vec<(String, String)>>);
    impl super::super::metadata::MetadataSource for Excluding {
        fn snapshot(&self) -> BTreeMap<String, super::super::state::IndexMetadata> {
            BTreeMap::new()
        }
        fn voting_exclusions(&self) -> Vec<(String, String)> {
            self.0.lock().clone()
        }
    }

    #[test]
    fn an_excluded_node_leaves_the_vote_and_the_rest_carry_on_without_it() {
        let ex = std::sync::Arc::new(Excluding(parking_lot::Mutex::new(Vec::new())));
        let mut sim = Sim::new(6);
        let names = ["a", "b", "c"];
        let ids: Vec<NodeId> = names.iter().map(|n| NodeId((*n).into())).collect();
        for n in names {
            let src = ex.clone();
            let seeds: Vec<NodeId> = ids.iter().filter(|i| i.as_str() != n).cloned().collect();
            sim.add_node(
                NodeId(n.into()),
                Box::new(move |_| {
                    let initial = names.iter().map(|s| s.to_string()).collect();
                    let mut c = Coordinator::new(node(n), "c", "u", initial, seeds.clone());
                    c.metadata = Some(src.clone());
                    Box::new(c)
                }),
            );
        }
        sim.run_until(10_000);
        assert_settled(&sim, &ids);
        let s = committed_of(&sim, &ids[0]).unwrap();
        let leader = s.cluster_manager.clone().unwrap();
        // exclude the manager by name, as `_cluster/voting_config_exclusions` does
        ex.0.lock().push(("_absent_".into(), leader.as_str().to_string()));
        sim.run_until(20_000);
        assert_settled(&sim, &ids);
        let s = committed_of(&sim, &ids[0]).unwrap();
        assert!(!s.last_committed_config.contains(&leader), "{:?}", s.last_committed_config);
        assert_eq!(s.voting_config_exclusions.len(), 1);
        // it still leads until it goes; then the others elect without it
        assert_eq!(s.cluster_manager, Some(leader.clone()));
        sim.crash(&leader);
        sim.run_until(50_000);
        assert_settled(&sim, &ids);
        let s = committed_of(&sim, &ids.iter().find(|i| **i != leader).unwrap()).unwrap();
        assert_ne!(s.cluster_manager, Some(leader));
        check_invariants(&sim, &ids);
    }

    #[test]
    fn a_follower_that_crashes_rejoins_from_what_it_kept() {
        let (mut sim, ids) = cluster(3, &["m", "a", "b"], &["m"]);
        sim.run_until(10_000);
        assert_settled(&sim, &ids);
        let b = ids[2].clone();
        let before = committed_of(&sim, &b).unwrap().version;
        sim.crash(&b);
        sim.run_until(40_000);
        assert_eq!(committed_of(&sim, &ids[0]).unwrap().nodes.len(), 2);
        sim.restart(&b);
        sim.run_until(70_000);
        assert_settled(&sim, &ids);
        assert!(committed_of(&sim, &b).unwrap().version > before);
        // the term it kept came back with it: no election was needed
        assert_eq!(leaders_by_term(&sim).len(), 1, "{:?}", sim.notes);
    }

    #[test]
    fn index_metadata_is_published_when_it_changes_and_only_then() {
        use crate::cluster::metadata::MapSource;
        use crate::cluster::state::{IndexMetadata, ShardState};
        let source = std::sync::Arc::new(MapSource::new(BTreeMap::new()));
        let mut sim = Sim::new(5);
        let ids: Vec<NodeId> = ["m", "a"].iter().map(|n| NodeId((*n).into())).collect();
        for n in ["m", "a"] {
            let src = source.clone();
            let seeds: Vec<NodeId> = ids.iter().filter(|i| i.as_str() != n).cloned().collect();
            sim.add_node(
                NodeId(n.into()),
                Box::new(move |_| {
                    let mut c =
                        Coordinator::new(node(n), "c", "u", vec!["m".into()], seeds.clone());
                    c.metadata = Some(src.clone());
                    Box::new(c)
                }),
            );
        }
        sim.run_until(10_000);
        assert_settled(&sim, &ids);
        let v0 = versions(&sim, &ids);
        // nothing changed: no new versions while the manager polls
        sim.run_until(20_000);
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
        sim.run_until(30_000);
        for id in &ids {
            let s = committed_of(&sim, id).unwrap();
            assert!(s.indices.contains_key("logs"), "{id} has no logs");
            let copies: Vec<_> = s.routing.shards_of("logs").collect();
            assert_eq!(copies.len(), 4);
            assert!(
                copies.iter().filter(|c| c.primary).all(|c| c.state == ShardState::Started
                    && c.node.as_ref() == Some(&NodeId("m".into())))
            );
            // the replicas went to the other node, started there, and are in sync
            assert!(
                copies.iter().filter(|c| !c.primary).all(|c| c.state == ShardState::Started
                    && c.node.as_ref() == Some(&NodeId("a".into()))),
                "{copies:?}"
            );
            assert_eq!(s.indices["logs"].in_sync_allocations[&0].len(), 2);
        }
        // and goes away
        source.0.lock().clear();
        sim.run_until(40_000);
        for id in &ids {
            assert!(committed_of(&sim, id).unwrap().indices.is_empty(), "{id} still has indices");
        }
    }

    /// Three data nodes, an index with a replica whose node crashes: the
    /// replica waits out the delay, then is placed again.
    #[test]
    fn a_lost_replica_is_placed_again_after_its_delay() {
        use crate::cluster::metadata::MapSource;
        use crate::cluster::state::{IndexMetadata, ShardState};
        let source = std::sync::Arc::new(MapSource::new(BTreeMap::new()));
        source.0.lock().insert(
            "d".into(),
            IndexMetadata {
                name: "d".into(),
                uuid: "u".into(),
                version: 1,
                mapping_version: 1,
                settings_version: 1,
                aliases_version: 1,
                state: "open".into(),
                settings: json!({"index": {"number_of_shards": "1", "number_of_replicas": "1", "unassigned": {"node_left": {"delayed_timeout": "5s"}}}}),
                mappings: json!({}),
                aliases: json!({}),
                number_of_shards: 1,
                number_of_replicas: 1,
                primary_terms: BTreeMap::new(),
                in_sync_allocations: BTreeMap::new(),
                creation_date: 0,
            },
        );
        let mut sim = Sim::new(21);
        let names = ["m", "a", "b"];
        let ids: Vec<NodeId> = names.iter().map(|n| NodeId((*n).into())).collect();
        for n in names {
            let src = source.clone();
            let seeds: Vec<NodeId> = ids.iter().filter(|i| i.as_str() != n).cloned().collect();
            sim.add_node(
                NodeId(n.into()),
                Box::new(move |_| {
                    let mut c =
                        Coordinator::new(node(n), "c", "u", vec!["m".into()], seeds.clone());
                    if n == "m" {
                        c.metadata = Some(src.clone());
                    }
                    Box::new(c)
                }),
            );
        }
        sim.run_until(15_000);
        assert_settled(&sim, &ids);
        let s = committed_of(&sim, &ids[0]).unwrap();
        let replica = s.routing.shards_of("d").find(|c| !c.primary).unwrap().clone();
        assert_eq!(replica.state, ShardState::Started, "{replica:?}");
        let holder = replica.node.clone().unwrap();
        assert_ne!(holder, NodeId("m".into()));
        sim.crash(&holder);
        // the node is dropped after its checks, then the replica waits
        sim.run_until(22_000);
        let s = committed_of(&sim, &ids[0]).unwrap();
        let replica = s.routing.shards_of("d").find(|c| !c.primary).unwrap().clone();
        assert_eq!(replica.state, ShardState::Unassigned, "{replica:?}");
        assert!(replica.unassigned.as_ref().unwrap().delayed);
        assert_eq!(replica.unassigned.as_ref().unwrap().reason, "NODE_LEFT");
        // five seconds later it is on the remaining node
        sim.run_until(40_000);
        let s = committed_of(&sim, &ids[0]).unwrap();
        let replica = s.routing.shards_of("d").find(|c| !c.primary).unwrap().clone();
        assert_eq!(replica.state, ShardState::Started, "{replica:?} {:?}", sim.notes);
        assert_ne!(replica.node, Some(holder));
    }

    fn index_meta(name: &str, shards: u32, replicas: u32) -> crate::cluster::state::IndexMetadata {
        crate::cluster::state::IndexMetadata {
            name: name.into(),
            uuid: format!("{name}-uuid"),
            version: 1,
            mapping_version: 1,
            settings_version: 1,
            aliases_version: 1,
            state: "open".into(),
            settings: json!({"index": {"number_of_shards": shards.to_string(), "number_of_replicas": replicas.to_string(), "unassigned": {"node_left": {"delayed_timeout": "1s"}}}}),
            mappings: json!({}),
            aliases: json!({}),
            number_of_shards: shards,
            number_of_replicas: replicas,
            primary_terms: BTreeMap::new(),
            in_sync_allocations: BTreeMap::new(),
            creation_date: 0,
        }
    }

    /// Every node has a store of its own (a map); the manager's holds an index.
    fn cluster_with_stores(
        seed: u64,
        names: &[&'static str],
        holder: &str,
        index: &str,
        shards: u32,
        replicas: u32,
    ) -> (Sim, Vec<NodeId>) {
        use crate::cluster::metadata::MapSource;
        let mut sim = Sim::new(seed);
        let ids: Vec<NodeId> = names.iter().map(|n| NodeId((*n).into())).collect();
        for n in names {
            let seeds: Vec<NodeId> = ids.iter().filter(|i| i.as_str() != *n).cloned().collect();
            let initial: Vec<String> = names.iter().map(|s| s.to_string()).collect();
            let mut map = BTreeMap::new();
            if *n == holder {
                map.insert(index.to_string(), index_meta(index, shards, replicas));
            }
            let src = std::sync::Arc::new(MapSource::new(map));
            let n: &'static str = n;
            sim.add_node(
                NodeId(n.into()),
                Box::new(move |_| {
                    let mut c = Coordinator::new(node(n), "c", "u", initial.clone(), seeds.clone());
                    c.metadata = Some(src.clone());
                    Box::new(c)
                }),
            );
        }
        (sim, ids)
    }

    /// The balancer moves primaries too: the copy that lands keeps being the
    /// primary, and the copy it came from is gone.
    #[test]
    fn a_primary_moved_by_the_balancer_stays_the_primary() {
        use crate::cluster::state::ShardState;
        let (mut sim, ids) = cluster_with_stores(31, &["a", "b", "c"], "a", "m", 6, 0);
        sim.run_until(60_000);
        assert_settled(&sim, &ids);
        let s = committed_of(&sim, &ids[0]).unwrap();
        let copies: Vec<_> = s.routing.shards_of("m").collect();
        assert_eq!(copies.len(), 6, "{copies:?}");
        assert!(copies.iter().all(|c| c.primary && c.state == ShardState::Started), "{copies:?}");
        let on_a = copies.iter().filter(|c| c.node == Some(NodeId("a".into()))).count();
        assert!(on_a <= 4, "the balancer moved nothing off a: {copies:?}");
        assert!(sim.notes.iter().any(|(_, _, t)| t.contains("relocating=1")), "{:?}", sim.notes);
    }

    /// The manager that made an index dies: the next manager still
    /// publishes it, and a replica of it becomes the primary.
    #[test]
    fn an_index_outlives_the_manager_that_made_it() {
        use crate::cluster::state::ShardState;
        let (mut sim, ids) = cluster_with_stores(32, &["a", "b", "c"], "a", "keep", 1, 1);
        sim.run_until(20_000);
        assert_settled(&sim, &ids);
        let s = committed_of(&sim, &ids[0]).unwrap();
        let leader = s.cluster_manager.clone().unwrap();
        // the index was made on a; a may or may not be the manager
        let holder = NodeId("a".into());
        let victim = if leader == holder { holder.clone() } else { holder.clone() };
        assert_eq!(s.routing.primary("keep", 0).unwrap().node, Some(holder.clone()));
        sim.crash(&victim);
        sim.run_until(60_000);
        let up: Vec<NodeId> = ids.iter().filter(|i| **i != victim).cloned().collect();
        assert_settled(&sim, &ids);
        let s = committed_of(&sim, &up[0]).unwrap();
        assert!(
            s.indices.contains_key("keep"),
            "the index was lost with its maker: {:?}",
            s.indices.keys()
        );
        let p = s.routing.primary("keep", 0).unwrap();
        assert_eq!(p.state, ShardState::Started);
        assert_ne!(p.node, Some(victim.clone()));
        assert_eq!(s.indices["keep"].primary_terms[&0], 2, "the promoted copy is in a new term");
        // the copy on the other survivor was placed and started
        let copies: Vec<_> = s.routing.shards_of("keep").collect();
        assert_eq!(copies.len(), 2);
        assert!(copies.iter().all(|c| c.state == ShardState::Started), "{copies:?}");
    }

    #[test]
    fn versions_only_rise_and_the_seed_repeats() {
        let (mut x, ids) = cluster(9, &["a", "b", "c", "d"], &["a", "b", "c", "d"]);
        x.network.drop_rate = 0.2;
        let mut last: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for t in (1_000..=60_000).step_by(1_000) {
            x.run_until(t);
            for (n, term, v, _) in versions(&x, &ids) {
                let prev = last.insert(n.clone(), (term, v)).unwrap_or((0, 0));
                assert!((term, v) >= prev, "{n} went from {prev:?} to {:?}", (term, v));
            }
        }
        check_invariants(&x, &ids);
        let (mut y, _) = cluster(9, &["a", "b", "c", "d"], &["a", "b", "c", "d"]);
        y.network.drop_rate = 0.2;
        y.run_until(60_000);
        assert_eq!(x.trace, y.trace);
    }

    #[test]
    fn crashes_and_losses_across_seeds_keep_the_invariants() {
        let names = ["a", "b", "c", "d", "e"];
        for seed in 10..16 {
            let (mut sim, ids) = cluster(seed, &names, &names);
            sim.network.drop_rate = 0.1;
            sim.network.min_latency = 5;
            sim.network.max_latency = 80;
            let mut rng = Rng::new(seed);
            let mut t = 5_000;
            // a storm: nodes crash and come back while messages go missing
            for _ in 0..6 {
                let victim = ids[rng.range(0, 4) as usize].clone();
                sim.run_until(t);
                if sim.is_up(&victim) {
                    sim.crash(&victim);
                } else {
                    sim.restart(&victim);
                }
                t += 5_000 + rng.range(0, 5_000);
                check_invariants(&sim, &ids);
            }
            for id in &ids {
                if !sim.is_up(id) {
                    sim.restart(id);
                }
            }
            sim.network.drop_rate = 0.0;
            sim.run_until(t + 90_000);
            assert_settled(&sim, &ids);
            let leaders: BTreeSet<&NodeId> = ids
                .iter()
                .filter(|i| {
                    committed_of(&sim, i)
                        .map(|s| s.cluster_manager.as_ref() == Some(*i))
                        .unwrap_or(false)
                })
                .collect();
            assert_eq!(leaders.len(), 1, "seed {seed}: {leaders:?}");
        }
    }
}
