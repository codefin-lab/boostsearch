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
fn load_durable(dir: Option<&std::path::Path>) -> Durable {
    let mut d = Durable::default();
    if let Some(dir) = dir {
        if let Ok(bytes) = std::fs::read(dir.join("_state").join("cluster_state.json")) {
            d.entries.insert("cluster_state".into(), bytes);
        }
    }
    d
}

fn save_durable(dir: Option<&std::path::Path>, d: &Durable) {
    let Some(dir) = dir else { return };
    if let Some(bytes) = d.entries.get("cluster_state") {
        let path = dir.join("_state").join("cluster_state.json");
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(dir));
        let _ = std::fs::write(path, bytes);
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
        let me = transport.local();
        tokio::spawn(async move {
            let mut durable = load_durable(data_dir.as_deref());
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
                let outputs = logic.handle(input, clock.as_ref(), &mut durable);
                save_durable(data_dir.as_deref(), &durable);
                *shared.state.write() = logic.state().clone();
                *shared.mode.write() = format!("{:?}", logic.mode);
                for o in outputs {
                    match o {
                        Output::Send { to, envelope } => {
                            let _ = transport.send(&to, envelope);
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
