//! The transport production uses: TCP, one framed connection per peer,
//! opened on demand and reopened when it drops.
//!
//! A connection starts with a handshake: each side sends an envelope for
//! `internal:transport/handshake` carrying its identity, so a connection is
//! known by the node at the other end rather than by an address. Messages
//! for a node whose address is not yet known cannot be sent; discovery
//! (6.3) learns addresses from seed hosts and the cluster state.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use super::node::NodeIdentity;
use super::transport::{
    Envelope, FrameError, Handler, Kind, MAX_FRAME, NodeId, SendError, Transport,
};

pub const HANDSHAKE: &str = "internal:transport/handshake";

/// What one side tells the other when a connection opens.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Hello {
    pub node_id: NodeId,
    pub ephemeral_id: NodeId,
    pub name: String,
    pub cluster_name: String,
    pub transport_address: String,
    pub roles: Vec<String>,
}

pub struct TcpTransport {
    me: Hello,
    /// addresses known for peers, learned from seeds, handshakes and state
    addresses: RwLock<HashMap<NodeId, String>>,
    /// one outbound queue per connected peer
    peers: Mutex<HashMap<NodeId, mpsc::UnboundedSender<Envelope>>>,
    handler: RwLock<Option<Arc<dyn Handler>>>,
    /// peers seen through a handshake, with what they said
    known: RwLock<HashMap<NodeId, Hello>>,
}

impl TcpTransport {
    pub fn new(identity: &NodeIdentity) -> Arc<TcpTransport> {
        Arc::new(TcpTransport {
            me: Hello {
                node_id: identity.id.clone(),
                ephemeral_id: identity.ephemeral_id.clone(),
                name: identity.name.clone(),
                cluster_name: identity.cluster_name.clone(),
                transport_address: identity.transport_address.clone(),
                roles: identity.roles.clone(),
            },
            addresses: RwLock::new(HashMap::new()),
            peers: Mutex::new(HashMap::new()),
            handler: RwLock::new(None),
            known: RwLock::new(HashMap::new()),
        })
    }

    pub fn hello(&self) -> &Hello {
        &self.me
    }

    /// Tell the transport where a node lives.
    pub fn learn_address(&self, node: NodeId, address: String) {
        self.addresses.write().insert(node, address);
    }

    /// The nodes that have shaken hands with this one.
    pub fn known_peers(&self) -> Vec<Hello> {
        self.known.read().values().cloned().collect()
    }

    /// Listen for connections; runs for the life of the node.
    pub async fn listen(self: Arc<Self>, bind: &str) -> anyhow::Result<()> {
        let listener = TcpListener::bind(bind).await?;
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => continue,
            };
            let me = self.clone();
            tokio::spawn(async move {
                let _ = me.serve_connection(stream, None).await;
            });
        }
    }

    /// Open a connection to an address, shake hands, and keep it.
    pub async fn connect(self: Arc<Self>, address: &str) -> anyhow::Result<Hello> {
        let stream = TcpStream::connect(address).await?;
        stream.set_nodelay(true)?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let me = self.clone();
        tokio::spawn(async move {
            let _ = me.serve_connection(stream, Some(tx)).await;
        });
        Ok(rx.await?)
    }

    /// One connection: say hello, hear theirs, then pump frames both ways.
    async fn serve_connection(
        self: Arc<Self>,
        stream: TcpStream,
        tell: Option<tokio::sync::oneshot::Sender<Hello>>,
    ) -> anyhow::Result<()> {
        let (mut rd, mut wr) = stream.into_split();
        // hello first, both ways
        let hello =
            Envelope::request(HANDSHAKE, self.me.node_id.clone(), 0, serde_json::to_vec(&self.me)?);
        wr.write_all(&hello.encode()).await?;
        let theirs = read_frame(&mut rd).await?;
        if theirs.action != HANDSHAKE {
            anyhow::bail!("peer did not shake hands");
        }
        let peer: Hello = serde_json::from_slice(&theirs.body)?;
        if peer.cluster_name != self.me.cluster_name {
            anyhow::bail!("peer is in cluster {} not {}", peer.cluster_name, self.me.cluster_name);
        }
        let peer_id = peer.node_id.clone();
        self.addresses.write().insert(peer_id.clone(), peer.transport_address.clone());
        self.known.write().insert(peer_id.clone(), peer.clone());
        let (tx, mut rx) = mpsc::unbounded_channel::<Envelope>();
        self.peers.lock().insert(peer_id.clone(), tx);
        if let Some(t) = tell {
            let _ = t.send(peer.clone());
        }
        // writer
        let writer = tokio::spawn(async move {
            while let Some(env) = rx.recv().await {
                if wr.write_all(&env.encode()).await.is_err() {
                    break;
                }
            }
        });
        // reader
        let me = self.clone();
        let reader = async move {
            loop {
                match read_frame(&mut rd).await {
                    Ok(env) => {
                        if let Some(h) = me.handler.read().clone() {
                            h.handle(env);
                        }
                    }
                    Err(_) => break,
                }
            }
        };
        reader.await;
        writer.abort();
        self.peers.lock().remove(&peer_id);
        Ok(())
    }

    /// A connection to the node, opened if there is none and its address
    /// is known.
    async fn ensure_peer(
        self: Arc<Self>,
        to: &NodeId,
    ) -> Result<mpsc::UnboundedSender<Envelope>, SendError> {
        if let Some(tx) = self.peers.lock().get(to) {
            return Ok(tx.clone());
        }
        let Some(addr) = self.addresses.read().get(to).cloned() else {
            return Err(SendError::Unreachable(to.clone()));
        };
        match self.clone().connect(&addr).await {
            Ok(_) => {
                self.peers.lock().get(to).cloned().ok_or_else(|| SendError::Unreachable(to.clone()))
            }
            Err(_) => Err(SendError::Unreachable(to.clone())),
        }
    }
}

impl Transport for TcpTransport {
    fn local(&self) -> NodeId {
        self.me.node_id.clone()
    }

    fn send(&self, to: &NodeId, envelope: Envelope) -> Result<(), SendError> {
        if let Some(tx) = self.peers.lock().get(to) {
            return tx.send(envelope).map_err(|_| SendError::Closed);
        }
        // no connection yet: open one on the runtime and send when it is up
        let me: Arc<TcpTransport> = match self.self_arc() {
            Some(a) => a,
            None => return Err(SendError::Unreachable(to.clone())),
        };
        let to = to.clone();
        tokio::spawn(async move {
            if let Ok(tx) = me.ensure_peer(&to).await {
                let _ = tx.send(envelope);
            }
        });
        Ok(())
    }

    fn set_handler(&self, handler: Arc<dyn Handler>) {
        *self.handler.write() = Some(handler);
    }
}

impl TcpTransport {
    /// The transport keeps a weak handle to itself so `send` can spawn.
    fn self_arc(&self) -> Option<Arc<TcpTransport>> {
        SELF.with(|s| s.borrow().as_ref().and_then(|w| w.upgrade()))
    }

    /// Remember the handle, once, after construction.
    pub fn register(self: &Arc<Self>) {
        SELF.with(|s| *s.borrow_mut() = Some(Arc::downgrade(self)));
        let weak = Arc::downgrade(self);
        *GLOBAL.lock() = Some(weak);
    }
}

thread_local! {
    static SELF: std::cell::RefCell<Option<std::sync::Weak<TcpTransport>>> = const { std::cell::RefCell::new(None) };
}
static GLOBAL: Mutex<Option<std::sync::Weak<TcpTransport>>> = Mutex::new(None);

/// The node's transport, from anywhere on the process.
pub fn global() -> Option<Arc<TcpTransport>> {
    GLOBAL.lock().as_ref().and_then(|w| w.upgrade())
}

async fn read_frame(rd: &mut tokio::net::tcp::OwnedReadHalf) -> anyhow::Result<Envelope> {
    let mut len = [0u8; 4];
    rd.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        anyhow::bail!(FrameError::TooLong(len));
    }
    let mut payload = vec![0u8; len];
    rd.read_exact(&mut payload).await?;
    let env = Envelope::decode(&payload)?;
    let _ = Kind::Request;
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(name: &str, port: u16) -> NodeIdentity {
        let mut id = NodeIdentity::load(&serde_json::Value::Null, None, "127.0.0.1:9200");
        id.name = name.into();
        id.transport_address = format!("127.0.0.1:{port}");
        id.transport_bind = format!("127.0.0.1:{port}");
        id
    }

    struct Seen(Mutex<Vec<Envelope>>);
    impl Handler for Seen {
        fn handle(&self, e: Envelope) {
            self.0.lock().push(e);
        }
    }

    #[tokio::test]
    async fn two_nodes_shake_hands_and_talk() {
        let a = TcpTransport::new(&identity("a", 39301));
        let b = TcpTransport::new(&identity("b", 39302));
        let seen = Arc::new(Seen(Mutex::new(Vec::new())));
        b.set_handler(seen.clone());
        let bl = b.clone();
        tokio::spawn(async move {
            let _ = bl.listen("127.0.0.1:39302").await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let hello = a.clone().connect("127.0.0.1:39302").await.unwrap();
        assert_eq!(hello.name, "b");
        assert_eq!(hello.node_id, b.local());
        assert_eq!(a.known_peers().len(), 1);
        // now a message from a to b by node id
        a.send(&b.local(), Envelope::request("internal:ping", a.local(), 7, b"{}".to_vec()))
            .unwrap();
        for _ in 0..50 {
            if !seen.0.lock().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let got = seen.0.lock();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].action, "internal:ping");
        assert_eq!(got[0].from, a.local());
        assert_eq!(got[0].request_id, 7);
    }
}
