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
    /// the outbound queues of the connections open to each peer: two nodes
    /// that dial each other at once hold two, and either serves
    peers: Mutex<HashMap<NodeId, Vec<mpsc::UnboundedSender<Envelope>>>>,
    /// the transport's own handle, so `send` can open a connection
    self_weak: std::sync::Weak<TcpTransport>,
    /// peers this node is cut off from: a partition made real at this end,
    /// for the chaos runs (`BOOSTSEARCH_CHAOS=1`)
    cut: RwLock<std::collections::HashSet<NodeId>>,
    handler: RwLock<Option<Arc<dyn Handler>>>,
    /// peers seen through a handshake, with what they said
    known: RwLock<HashMap<NodeId, Hello>>,
}

impl TcpTransport {
    pub fn new(identity: &NodeIdentity) -> Arc<TcpTransport> {
        Arc::new_cyclic(|weak| TcpTransport {
            self_weak: weak.clone(),
            cut: RwLock::new(std::collections::HashSet::new()),
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

    /// Cut this node off from the named peers: nothing is sent to them,
    /// nothing read from them, and their connections are closed.
    pub fn cut(&self, peers: &[NodeId]) {
        {
            let mut c = self.cut.write();
            for p in peers {
                c.insert(p.clone());
            }
        }
        let mut open = self.peers.lock();
        for p in peers {
            open.remove(p);
        }
    }

    /// Every cut mended.
    pub fn heal(&self) {
        self.cut.write().clear();
    }

    pub fn is_cut(&self, peer: &NodeId) -> bool {
        self.cut.read().contains(peer)
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
        if self.is_cut(&peer_id) {
            anyhow::bail!("cut off from {peer_id}");
        }
        self.addresses.write().insert(peer_id.clone(), peer.transport_address.clone());
        self.known.write().insert(peer_id.clone(), peer.clone());
        let (tx, mut rx) = mpsc::unbounded_channel::<Envelope>();
        self.peers.lock().entry(peer_id.clone()).or_default().push(tx.clone());
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
                        // a partition made real: a frame from a cut peer is lost
                        // on the wire, and the connection stays as it would
                        if me.is_cut(&env.from) {
                            continue;
                        }
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
        // this connection's queue goes, and no other's
        {
            let mut peers = self.peers.lock();
            if let Some(list) = peers.get_mut(&peer_id) {
                list.retain(|s| !s.same_channel(&tx));
                if list.is_empty() {
                    peers.remove(&peer_id);
                }
            }
        }
        if std::env::var("BOOSTSEARCH_CLUSTER_DEBUG").map(|v| v == "2").unwrap_or(false) {
            eprintln!("transport {}: connection with {peer_id} closed", self.me.node_id);
        }
        Ok(())
    }

    /// A connection to the node, opened if there is none and its address
    /// is known.
    /// A live queue to the node, if a connection is open.
    fn queue_to(&self, to: &NodeId) -> Option<mpsc::UnboundedSender<Envelope>> {
        if self.is_cut(to) {
            return None;
        }
        self.peers.lock().get(to).and_then(|list| list.iter().find(|s| !s.is_closed()).cloned())
    }

    async fn ensure_peer(
        self: Arc<Self>,
        to: &NodeId,
    ) -> Result<mpsc::UnboundedSender<Envelope>, SendError> {
        if let Some(tx) = self.queue_to(to) {
            return Ok(tx);
        }
        if self.is_cut(to) {
            return Err(SendError::Unreachable(to.clone()));
        }
        let Some(addr) = self.addresses.read().get(to).cloned() else {
            return Err(SendError::Unreachable(to.clone()));
        };
        match self.clone().connect(&addr).await {
            Ok(_) => self.queue_to(to).ok_or_else(|| SendError::Unreachable(to.clone())),
            Err(_) => Err(SendError::Unreachable(to.clone())),
        }
    }
}

impl Transport for TcpTransport {
    fn local(&self) -> NodeId {
        self.me.node_id.clone()
    }

    fn send(&self, to: &NodeId, envelope: Envelope) -> Result<(), SendError> {
        // a partition loses frames without a word: nothing fails fast, the
        // way a real one behaves; the follower checks are what notice it
        if self.is_cut(to) {
            return Ok(());
        }
        if let Some(tx) = self.queue_to(to) {
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
    /// The transport's own handle, so `send` can spawn a connection.
    fn self_arc(&self) -> Option<Arc<TcpTransport>> {
        self.self_weak.upgrade()
    }

    /// Make this the node's transport, the one `global()` hands out.
    pub fn register(self: &Arc<Self>) {
        *GLOBAL.lock() = Some(Arc::downgrade(self));
    }
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
    async fn three_nodes_dial_each_other_at_once_and_all_pairs_talk_both_ways() {
        let ports = [39311u16, 39312, 39313];
        let names = ["a", "b", "c"];
        let ts: Vec<Arc<TcpTransport>> =
            (0..3).map(|i| TcpTransport::new(&identity(names[i], ports[i]))).collect();
        let seen: Vec<Arc<Seen>> = (0..3).map(|_| Arc::new(Seen(Mutex::new(Vec::new())))).collect();
        for i in 0..3 {
            ts[i].set_handler(seen[i].clone());
            let l = ts[i].clone();
            tokio::spawn(async move {
                let _ = l.listen(&format!("127.0.0.1:{}", ports[i])).await;
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // everyone dials everyone else at the same moment
        let mut dials = Vec::new();
        for i in 0..3 {
            for j in 0..3 {
                if i != j {
                    let t = ts[i].clone();
                    dials.push(tokio::spawn(async move {
                        t.connect(&format!("127.0.0.1:{}", ports[j])).await.unwrap()
                    }));
                }
            }
        }
        for d in dials {
            d.await.unwrap();
        }
        // then every pair talks both ways, twice, with a pause between
        for round in 0..2u64 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            for i in 0..3 {
                for j in 0..3 {
                    if i != j {
                        ts[i]
                            .send(
                                &ts[j].local(),
                                Envelope::request(
                                    "internal:ping",
                                    ts[i].local(),
                                    round * 10 + i as u64,
                                    vec![],
                                ),
                            )
                            .unwrap();
                    }
                }
            }
        }
        for _ in 0..100 {
            if seen.iter().all(|s| s.0.lock().len() == 4) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        for (i, s) in seen.iter().enumerate() {
            let got = s.0.lock();
            assert_eq!(
                got.len(),
                4,
                "{} got {:?}",
                names[i],
                got.iter().map(|e| (e.from.clone(), e.request_id)).collect::<Vec<_>>()
            );
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
