//! The production driver: the same node logic the simulation runs, on
//! tokio, over the TCP transport, with real timers.
//!
//! Inputs come from the transport's handler and from timers; outputs go
//! out through the transport or set timers. The committed cluster state is
//! copied out after every input so the HTTP handlers can read it without
//! touching the logic.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use tokio::sync::mpsc;

use super::clock::{Clock, Millis};
use super::coordinator::Coordinator;
use super::sim::{Durable, Input, NodeLogic, Output};
use super::state::ClusterState;
use super::tcp::TcpTransport;
use super::transport::{Envelope, Handler, NodeId, Transport};

/// What the rest of the node reads: the last committed cluster state.
pub struct Shared {
    pub state: RwLock<ClusterState>,
    pub mode: RwLock<String>,
}

pub struct Runtime {
    inputs: mpsc::UnboundedSender<Input>,
    pub shared: Arc<Shared>,
}

struct Inbox(mpsc::UnboundedSender<Input>);

impl Handler for Inbox {
    fn handle(&self, envelope: Envelope) {
        let _ = self.0.send(Input::Message(envelope));
    }
}

/// Durable state kept in the data directory as one file per key.
const DURABLE_KEYS: [&str; 3] = [
    super::coordinator::D_COMMITTED,
    super::coordinator::D_ACCEPTED,
    super::coordinator::D_TERM,
];

fn load_durable(dir: Option<&std::path::Path>) -> Durable {
    let mut d = Durable::default();
    if let Some(dir) = dir {
        for key in DURABLE_KEYS {
            if let Ok(bytes) = std::fs::read(dir.join("_state").join(format!("{key}.json"))) {
                d.entries.insert(key.into(), bytes);
            }
        }
    }
    d
}

/// Write what changed since the last save, and nothing else.
fn save_durable(dir: Option<&std::path::Path>, d: &Durable, written: &mut BTreeMap<String, Vec<u8>>) {
    let Some(dir) = dir else { return };
    for key in DURABLE_KEYS {
        let Some(bytes) = d.entries.get(key) else { continue };
        if written.get(key) == Some(bytes) {
            continue;
        }
        let path = dir.join("_state").join(format!("{key}.json"));
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(dir));
        if std::fs::write(&path, bytes).is_ok() {
            written.insert(key.into(), bytes.clone());
        }
    }
}

impl Runtime {
    /// Start the coordinator on this node, listening through the transport.
    pub fn start(
        transport: Arc<TcpTransport>,
        clock: Arc<dyn Clock>,
        mut logic: Coordinator,
        data_dir: Option<std::path::PathBuf>,
    ) -> Arc<Runtime> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Input>();
        transport.set_handler(Arc::new(Inbox(tx.clone())));
        let shared = Arc::new(Shared {
            state: RwLock::new(logic.state().clone()),
            mode: RwLock::new(format!("{:?}", logic.mode)),
        });
        let rt = Arc::new(Runtime { inputs: tx.clone(), shared: shared.clone() });
        let timers: Arc<Mutex<BTreeMap<u64, u64>>> = Arc::new(Mutex::new(BTreeMap::new()));
        // seed hosts reached, and those being dialled right now
        let dialled: Arc<Mutex<std::collections::HashSet<String>>> = Arc::default();
        let dialling: Arc<Mutex<std::collections::HashSet<String>>> = Arc::default();
        let me = transport.local();
        tokio::spawn(async move {
            let mut durable = load_durable(data_dir.as_deref());
            let mut written: BTreeMap<String, Vec<u8>> = durable.entries.clone();
            let mut epoch: u64 = 0;
            // the start input, then the loop
            let mut pending: Vec<Input> = vec![Input::Start];
            loop {
                let input = match pending.pop() {
                    Some(i) => i,
                    None => match rx.recv().await {
                        Some(i) => i,
                        None => break,
                    },
                };
                let trace = std::env::var("BOOSTSEARCH_CLUSTER_DEBUG").map(|v| v == "2").unwrap_or(false);
                if trace {
                    let what = match &input {
                        Input::Start => "start".to_string(),
                        Input::Timer(id) => format!("timer {id}"),
                        Input::Peer(n) => format!("peer {}", n.id),
                        Input::Message(e) => format!("{:?} {} from {}", e.kind, e.action, e.from),
                    };
                    eprintln!("cluster {me} <- {what}");
                }
                let outputs = logic.handle(input, clock.as_ref(), &mut durable);
                if trace {
                    for o in &outputs {
                        let what = match o {
                            Output::Send { to, envelope } => format!("{:?} {} to {to}", envelope.kind, envelope.action),
                            Output::Timer { id, after } => format!("timer {id} in {after}ms"),
                            Output::Note(t) => format!("note {t}"),
                            Output::Peer { id, address } => format!("peer {id} at {address}"),
                            Output::Dial(a) => format!("dial {a}"),
                        };
                        eprintln!("cluster {me} -> {what}");
                    }
                }
                save_durable(data_dir.as_deref(), &durable, &mut written);
                *shared.state.write() = logic.state().clone();
                *shared.mode.write() = format!("{:?}", logic.mode);
                for o in outputs {
                    match o {
                        Output::Send { to, envelope } => {
                            if let Err(e) = transport.send(&to, envelope) {
                                if trace {
                                    eprintln!("cluster {me}: send to {to} failed: {e:?}");
                                }
                            }
                        }
                        Output::Timer { id, after } => {
                            epoch += 1;
                            let my_epoch = epoch;
                            timers.lock().insert(id, my_epoch);
                            let tx = tx.clone();
                            let timers = timers.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_millis(after)).await;
                                // a timer reset since is not this one's to fire
                                if timers.lock().get(&id) == Some(&my_epoch) {
                                    let _ = tx.send(Input::Timer(id));
                                }
                            });
                        }
                        Output::Peer { id, address } => {
                            transport.learn_address(id, address);
                        }
                        Output::Dial(address) => {
                            if dialled.lock().contains(&address) || !dialling.lock().insert(address.clone()) {
                                continue;
                            }
                            let t = transport.clone();
                            let tx = tx.clone();
                            let dialled = dialled.clone();
                            let dialling = dialling.clone();
                            tokio::spawn(async move {
                                if let Ok(h) = t.connect(&address).await {
                                    dialled.lock().insert(address.clone());
                                    let _ = tx.send(Input::Peer(super::state::DiscoveryNode {
                                        id: h.node_id,
                                        name: h.name,
                                        ephemeral_id: h.ephemeral_id,
                                        transport_address: h.transport_address,
                                        roles: h.roles,
                                        attributes: BTreeMap::new(),
                                    }));
                                }
                                dialling.lock().remove(&address);
                            });
                        }
                        Output::Note(text) => {
                            if std::env::var("BOOSTSEARCH_CLUSTER_DEBUG").is_ok() {
                                eprintln!("cluster {me}: {text}");
                            }
                        }
                    }
                }
            }
        });
        rt
    }

    /// Hand the logic a message from inside the process (a client's request).
    pub fn inject(&self, envelope: Envelope) {
        let _ = self.inputs.send(Input::Message(envelope));
    }

    pub fn state(&self) -> ClusterState {
        self.shared.state.read().clone()
    }
}

/// Connect to the seed hosts and learn who is there, before the
/// coordinator goes looking for the manager by id.
pub async fn discover_seeds(transport: Arc<TcpTransport>, seeds: &[String]) -> Vec<NodeId> {
    let mut found = Vec::new();
    for host in seeds {
        let addr = if host.contains(':') { host.clone() } else { format!("{host}:9300") };
        if let Ok(hello) = transport.clone().connect(&addr).await {
            found.push(hello.node_id);
        }
    }
    found
}

#[allow(dead_code)]
fn _millis(m: Millis) -> Millis {
    m
}
