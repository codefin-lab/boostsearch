//! The cluster: several nodes that agree on where every shard lives and
//! carry writes to every copy.
//!
//! Written against a transport and a clock it does not own (ADR 0002), so
//! the whole thing runs in one thread under a seed in the simulation; with
//! one consistency mode shipped and the replication path taking its
//! acknowledgement policy as a parameter (ADR 0003).

pub mod clock;
pub mod node;
pub mod sim;
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
