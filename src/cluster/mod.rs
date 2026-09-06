//! The cluster: several nodes that agree on where every shard lives and
//! carry writes to every copy.
//!
//! Written against a transport and a clock it does not own (ADR 0002), so
//! the whole thing runs in one thread under a seed in the simulation; with
//! one consistency mode shipped and the replication path taking its
//! acknowledgement policy as a parameter (ADR 0003).

pub mod allocation;
pub mod clock;
pub mod coordinator;
pub mod forward;
pub mod metadata;
pub mod model;
pub mod node;
pub mod replication;
pub mod runtime;
pub mod search;
pub mod sim;
pub mod state;
pub mod tcp;
pub mod transport;

pub use clock::{Clock, ManualClock, SystemClock};
pub use node::NodeIdentity;
pub use transport::{Envelope, Handler, Kind, NodeId, SendError, Transport};

use std::sync::{Arc, OnceLock};

static IDENTITY: OnceLock<NodeIdentity> = OnceLock::new();

/// Fix this node's identity, once, at startup.
pub fn set_identity(id: NodeIdentity) {
    let _ = IDENTITY.set(id);
}

/// This node, as the settings and the data directory say; a default
/// identity while nothing has been set (tests, tools).
pub fn identity() -> &'static NodeIdentity {
    IDENTITY.get_or_init(|| NodeIdentity::load(&serde_json::Value::Null, None, "127.0.0.1:9200"))
}

/// The cluster's clock: the system's, unless the simulation set one.
pub fn clock() -> Arc<dyn Clock> {
    CLOCK.get_or_init(SystemClock::new).clone()
}

static CLOCK: OnceLock<Arc<dyn Clock>> = OnceLock::new();

pub fn set_clock(c: Arc<dyn Clock>) {
    let _ = CLOCK.set(c);
}

/// The cluster's uuid: for a single node, fixed by its data directory
/// alongside the node id (the cluster that node forms); a cluster of many
/// gets the one the first cluster manager committed (6.4).
pub fn cluster_uuid() -> String {
    CLUSTER_UUID.get_or_init(|| identity().cluster_uuid.clone()).clone()
}

static CLUSTER_UUID: OnceLock<String> = OnceLock::new();

/// The cluster state's own uuid: fresh each time a state is published.
pub fn state_uuid() -> String {
    STATE_UUID.get_or_init(|| NodeId::random().0).clone()
}

static STATE_UUID: OnceLock<String> = OnceLock::new();

static RUNTIME: OnceLock<Arc<runtime::Runtime>> = OnceLock::new();
static HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// The node's coordinator runtime, once it is up.
pub fn set_runtime(rt: Arc<runtime::Runtime>) {
    if let Ok(h) = tokio::runtime::Handle::try_current() {
        let _ = HANDLE.set(h);
    }
    let _ = RUNTIME.set(rt);
}

/// The runtime the node is running on, for the few places that must ask
/// another node from a thread that is not itself async.
pub fn handle() -> Option<tokio::runtime::Handle> {
    HANDLE.get().cloned().or_else(|| tokio::runtime::Handle::try_current().ok())
}

pub fn runtime() -> Option<Arc<runtime::Runtime>> {
    RUNTIME.get().cloned()
}

/// Is there a cluster manager this node is answering to? A node on its own
/// (no coordinator running at all) is its own authority and says yes.
pub fn has_manager() -> bool {
    runtime().map(|rt| rt.has_manager()).unwrap_or(true)
}

/// Whether this node is the one the cluster looks to.
///
/// Work that must happen once for the whole cluster -- moving indices along
/// under a policy, say -- asks this first. A node running on its own is its
/// own manager and says yes.
pub fn is_cluster_manager() -> bool {
    let state = current_state();
    match (&state.cluster_manager, runtime()) {
        (Some(manager), Some(_)) => *manager == identity().id,
        _ => true,
    }
}

/// The last committed cluster state, or a state of this node alone while
/// the coordinator has not started (tools, tests).
thread_local! {
    static OVERRIDE: std::cell::RefCell<Option<state::ClusterState>> = const { std::cell::RefCell::new(None) };
}

/// Render with a state that is not the committed one (a `_cluster/reroute`
/// dry run): `current_state()` answers with it while `f` runs.
pub fn with_state_override<R>(s: state::ClusterState, f: impl FnOnce() -> R) -> R {
    OVERRIDE.with(|o| *o.borrow_mut() = Some(s));
    let r = f();
    OVERRIDE.with(|o| *o.borrow_mut() = None);
    r
}

/// Read the committed state without copying it (the write path asks per
/// document); a node with nothing committed reads the synthetic state.
pub fn with_state<R>(f: impl FnOnce(&state::ClusterState) -> R) -> R {
    let overridden = OVERRIDE.with(|o| o.borrow().clone());
    if let Some(s) = overridden {
        return f(&s);
    }
    match runtime() {
        Some(rt) if rt.with_state(|s| s.version > 0) => rt.with_state(f),
        _ => f(&current_state()),
    }
}

/// The term a shard's primary is in, as the manager published it (1 until
/// a replica has taken over).
pub fn primary_term(index: &str, shard: u32) -> u64 {
    with_state(|s| {
        s.indices.get(index).and_then(|m| m.primary_terms.get(&shard).copied()).unwrap_or(1)
    })
}

pub fn current_state() -> state::ClusterState {
    if let Some(s) = OVERRIDE.with(|o| o.borrow().clone()) {
        return s;
    }
    if let Some(rt) = runtime() {
        let s = rt.state();
        if s.version > 0 {
            return s;
        }
    }
    let me = identity();
    let mut s = state::ClusterState::empty(&me.cluster_name, &cluster_uuid());
    s.version = 1;
    s.term = 1;
    s.cluster_uuid_committed = true;
    s.cluster_manager = Some(me.id.clone());
    s.nodes.insert(me.id.clone(), discovery_node());
    s.last_committed_config = vec![me.id.clone()];
    s.last_accepted_config = vec![me.id.clone()];
    s
}

/// This node as the cluster state describes it.
pub fn discovery_node() -> state::DiscoveryNode {
    let me = identity();
    state::DiscoveryNode {
        id: me.id.clone(),
        name: me.name.clone(),
        ephemeral_id: me.ephemeral_id.clone(),
        transport_address: me.transport_address.clone(),
        roles: me.roles.clone(),
        attributes: me
            .attributes
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
            .collect(),
    }
}
