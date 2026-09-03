//! The simulation: a whole cluster in one thread, on a clock and a
//! network a seed drives.
//!
//! A node here is a piece of logic that takes inputs -- a message, a
//! timer, a start -- and hands back outputs -- messages to send, timers to
//! set, notes for the invariant checks. The scheduler holds every pending
//! event in one queue ordered by time, and the seed decides message
//! latency, which messages a partition or a drop rate loses, and when a
//! crash or a clock jump happens. The same seed makes the same run.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;

use super::clock::{Clock, ManualClock, Millis};
use super::transport::{Envelope, Handler, NodeId, SendError, Transport};

/// A small, fast, seedable generator (splitmix64): enough for choosing
/// latencies and losses, and exactly repeatable.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `lo..=hi`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + self.next_u64() % (hi - lo + 1)
    }

    /// True with this probability.
    pub fn chance(&mut self, p: f64) -> bool {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }
}

/// What a node is told.
#[derive(Clone, Debug)]
pub enum Input {
    /// the node starts (or restarts, with its durable state back)
    Start,
    Message(Envelope),
    Timer(u64),
    /// a node the runtime reached by dialling an address it was asked to
    Peer(super::state::DiscoveryNode),
    /// a copy the host finished starting (or failed to), by allocation id
    ShardDone {
        allocation_id: String,
        result: Result<(), String>,
    },
}

/// What a node asks for.
#[derive(Clone, Debug)]
pub enum Output {
    Send {
        to: NodeId,
        envelope: Envelope,
    },
    /// fire `Input::Timer(id)` after this many milliseconds
    Timer {
        id: u64,
        after: Millis,
    },
    /// something the invariant checks may read
    Note(String),
    /// a node learned of, with the address it is reached at (the
    /// production transport keeps it; the simulation routes by id)
    Peer {
        id: NodeId,
        address: String,
    },
    /// an address to dial, for a seed host whose node is not yet known
    /// (the simulation knows every node by id and ignores it)
    Dial(String),
}

/// State a node keeps across a crash: what it wrote to disk.
#[derive(Clone, Debug, Default)]
pub struct Durable {
    pub entries: BTreeMap<String, Vec<u8>>,
}

/// A node's logic, without any I/O of its own.
pub trait NodeLogic: Send {
    fn handle(&mut self, input: Input, clock: &dyn Clock, durable: &mut Durable) -> Vec<Output>;
}

/// Makes a node's logic fresh, as a start after a crash does.
pub type Factory = Box<dyn Fn(NodeId) -> Box<dyn NodeLogic> + Send>;

#[derive(Debug)]
enum What {
    Deliver { to: NodeId, envelope: Envelope },
    Timer { node: NodeId, id: u64, epoch: u64 },
    Crash(NodeId),
    Restart(NodeId),
    Heal,
}

struct Event {
    at: Millis,
    seq: u64,
    what: What,
}

impl PartialEq for Event {
    fn eq(&self, o: &Self) -> bool {
        self.at == o.at && self.seq == o.seq
    }
}
impl Eq for Event {}
impl PartialOrd for Event {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Event {
    fn cmp(&self, o: &Self) -> Ordering {
        // earliest first: reverse for the max-heap
        o.at.cmp(&self.at).then_with(|| o.seq.cmp(&self.seq))
    }
}

struct SimNode {
    logic: Option<Box<dyn NodeLogic>>,
    factory: Factory,
    durable: Durable,
    /// this node's view of time: the simulation's plus its skew
    clock: Arc<SkewedClock>,
    /// timers set before a crash must not fire after a restart
    epoch: u64,
    up: bool,
}

/// A node's clock: the simulation's, offset by its skew.
pub struct SkewedClock {
    base: Arc<ManualClock>,
    skew: Mutex<i64>,
}

impl Clock for SkewedClock {
    fn now(&self) -> Millis {
        let s = *self.skew.lock();
        (self.base.now() as i64 + s).max(0) as u64
    }
    fn wall(&self) -> Millis {
        let s = *self.skew.lock();
        (self.base.wall() as i64 + s).max(0) as u64
    }
}

/// The network as the seed sees it.
#[derive(Clone, Debug)]
pub struct Network {
    pub min_latency: Millis,
    pub max_latency: Millis,
    /// the share of messages lost even with no partition
    pub drop_rate: f64,
    /// pairs that cannot hear each other while a partition stands
    partitions: Vec<(HashSet<NodeId>, HashSet<NodeId>)>,
}

impl Default for Network {
    fn default() -> Network {
        Network { min_latency: 1, max_latency: 10, drop_rate: 0.0, partitions: Vec::new() }
    }
}

impl Network {
    fn cut(&self, a: &NodeId, b: &NodeId) -> bool {
        self.partitions
            .iter()
            .any(|(x, y)| (x.contains(a) && y.contains(b)) || (x.contains(b) && y.contains(a)))
    }
}

/// The whole cluster, in one place.
pub struct Sim {
    pub rng: Rng,
    clock: Arc<ManualClock>,
    nodes: BTreeMap<NodeId, SimNode>,
    queue: BinaryHeap<Event>,
    seq: u64,
    pub network: Network,
    /// every note every node made, in order, for the checks
    pub notes: Vec<(Millis, NodeId, String)>,
    /// the trace of what happened, for telling two runs apart
    pub trace: Vec<String>,
    delivered: u64,
    dropped: u64,
}

impl Sim {
    pub fn new(seed: u64) -> Sim {
        Sim {
            rng: Rng::new(seed),
            clock: ManualClock::new(0),
            nodes: BTreeMap::new(),
            queue: BinaryHeap::new(),
            seq: 0,
            network: Network::default(),
            notes: Vec::new(),
            trace: Vec::new(),
            delivered: 0,
            dropped: 0,
        }
    }

    pub fn now(&self) -> Millis {
        self.clock.now()
    }

    fn push(&mut self, at: Millis, what: What) {
        self.seq += 1;
        self.queue.push(Event { at, seq: self.seq, what });
    }

    /// Add a node; it starts at once.
    pub fn add_node(&mut self, id: NodeId, factory: Factory) {
        let clock = Arc::new(SkewedClock { base: self.clock.clone(), skew: Mutex::new(0) });
        self.nodes.insert(
            id.clone(),
            SimNode {
                logic: None,
                factory,
                durable: Durable::default(),
                clock,
                epoch: 0,
                up: false,
            },
        );
        self.start(&id);
    }

    fn start(&mut self, id: &NodeId) {
        let outputs = {
            let n = self.nodes.get_mut(id).expect("node");
            n.logic = Some((n.factory)(id.clone()));
            n.up = true;
            n.epoch += 1;
            let clock = n.clock.clone();
            let logic = n.logic.as_mut().unwrap();
            logic.handle(Input::Start, clock.as_ref(), &mut n.durable)
        };
        self.trace.push(format!("{} start {id}", self.now()));
        self.apply(id, outputs);
    }

    fn apply(&mut self, from: &NodeId, outputs: Vec<Output>) {
        for o in outputs {
            match o {
                Output::Send { to, envelope } => self.route(from, &to, envelope),
                Output::Timer { id, after } => {
                    let epoch = self.nodes.get(from).map(|n| n.epoch).unwrap_or(0);
                    let at = self.now() + after;
                    self.push(at, What::Timer { node: from.clone(), id, epoch });
                }
                Output::Peer { .. } | Output::Dial(_) => {}
                Output::Note(text) => {
                    self.trace.push(format!("{} note {from} {text}", self.now()));
                    self.notes.push((self.now(), from.clone(), text));
                }
            }
        }
    }

    /// A message from one node to another, subject to the network.
    fn route(&mut self, from: &NodeId, to: &NodeId, envelope: Envelope) {
        if !self.nodes.contains_key(to) {
            self.dropped += 1;
            return;
        }
        if self.network.cut(from, to) || self.rng.chance(self.network.drop_rate) {
            self.dropped += 1;
            self.trace.push(format!("{} drop {from}->{to} {}", self.now(), envelope.action));
            return;
        }
        let latency = self.rng.range(self.network.min_latency, self.network.max_latency);
        let at = self.now() + latency;
        self.push(at, What::Deliver { to: to.clone(), envelope });
    }

    /// Cut the network between two sets of nodes.
    pub fn partition(&mut self, a: &[NodeId], b: &[NodeId]) {
        self.network.partitions.push((a.iter().cloned().collect(), b.iter().cloned().collect()));
        self.trace.push(format!("{} partition", self.now()));
    }

    /// Mend every partition.
    pub fn heal(&mut self) {
        self.network.partitions.clear();
        self.trace.push(format!("{} heal", self.now()));
    }

    /// Mend every partition at a later time.
    pub fn heal_at(&mut self, at: Millis) {
        self.push(at, What::Heal);
    }

    /// A node dies now: its logic and its timers are gone, its durable
    /// state stays.
    pub fn crash(&mut self, id: &NodeId) {
        if let Some(n) = self.nodes.get_mut(id) {
            n.logic = None;
            n.up = false;
            n.epoch += 1;
        }
        self.trace.push(format!("{} crash {id}", self.now()));
    }

    pub fn crash_at(&mut self, id: &NodeId, at: Millis) {
        self.push(at, What::Crash(id.clone()));
    }

    /// A crashed node comes back, from its durable state.
    pub fn restart(&mut self, id: &NodeId) {
        if self.nodes.get(id).map(|n| n.up).unwrap_or(true) {
            return;
        }
        self.start(id);
    }

    pub fn restart_at(&mut self, id: &NodeId, at: Millis) {
        self.push(at, What::Restart(id.clone()));
    }

    /// Move a node's clock off the true time by this much.
    pub fn skew(&mut self, id: &NodeId, by: i64) {
        if let Some(n) = self.nodes.get(id) {
            *n.clock.skew.lock() = by;
        }
        self.trace.push(format!("{} skew {id} {by}", self.now()));
    }

    pub fn is_up(&self, id: &NodeId) -> bool {
        self.nodes.get(id).map(|n| n.up).unwrap_or(false)
    }

    pub fn durable(&self, id: &NodeId) -> Option<&Durable> {
        self.nodes.get(id).map(|n| &n.durable)
    }

    pub fn node_ids(&self) -> Vec<NodeId> {
        self.nodes.keys().cloned().collect()
    }

    pub fn stats(&self) -> (u64, u64) {
        (self.delivered, self.dropped)
    }

    /// Let a node's logic see something without the network: a client
    /// request arriving at that node.
    pub fn inject(&mut self, to: &NodeId, envelope: Envelope) {
        self.push(self.now(), What::Deliver { to: to.clone(), envelope });
    }

    /// Run one event; false when nothing is left.
    pub fn step(&mut self) -> bool {
        let Some(ev) = self.queue.pop() else { return false };
        if ev.at > self.clock.now() {
            self.clock.set(ev.at);
        }
        match ev.what {
            What::Deliver { to, envelope } => {
                let outputs = {
                    let Some(n) = self.nodes.get_mut(&to) else { return true };
                    if !n.up {
                        self.dropped += 1;
                        return true;
                    }
                    let clock = n.clock.clone();
                    let logic = n.logic.as_mut().unwrap();
                    logic.handle(Input::Message(envelope), clock.as_ref(), &mut n.durable)
                };
                self.delivered += 1;
                self.apply(&to, outputs);
            }
            What::Timer { node, id, epoch } => {
                let outputs = {
                    let Some(n) = self.nodes.get_mut(&node) else { return true };
                    if !n.up || n.epoch != epoch {
                        return true;
                    }
                    let clock = n.clock.clone();
                    let logic = n.logic.as_mut().unwrap();
                    logic.handle(Input::Timer(id), clock.as_ref(), &mut n.durable)
                };
                self.apply(&node, outputs);
            }
            What::Crash(id) => self.crash(&id),
            What::Restart(id) => self.restart(&id),
            What::Heal => self.heal(),
        }
        true
    }

    /// Run everything due up to and including this time.
    pub fn run_until(&mut self, until: Millis) {
        while let Some(next) = self.queue.peek() {
            if next.at > until {
                break;
            }
            self.step();
        }
        if self.clock.now() < until {
            self.clock.set(until);
        }
    }

    /// Run until the queue is empty or this many events have happened.
    pub fn run(&mut self, max_events: usize) {
        for _ in 0..max_events {
            if !self.step() {
                break;
            }
        }
    }
}

/// A transport a piece of production-shaped code can be given inside
/// the simulation: sends become outputs the scheduler routes.
pub struct SimTransport {
    me: NodeId,
    outbox: Mutex<Vec<(NodeId, Envelope)>>,
    handler: Mutex<Option<Arc<dyn Handler>>>,
}

impl SimTransport {
    pub fn new(me: NodeId) -> Arc<SimTransport> {
        Arc::new(SimTransport { me, outbox: Mutex::new(Vec::new()), handler: Mutex::new(None) })
    }

    /// What was sent since the last take.
    pub fn take(&self) -> Vec<(NodeId, Envelope)> {
        std::mem::take(&mut *self.outbox.lock())
    }

    pub fn deliver(&self, envelope: Envelope) {
        if let Some(h) = self.handler.lock().clone() {
            h.handle(envelope);
        }
    }
}

impl Transport for SimTransport {
    fn local(&self) -> NodeId {
        self.me.clone()
    }
    fn send(&self, to: &NodeId, envelope: Envelope) -> Result<(), SendError> {
        self.outbox.lock().push((to.clone(), envelope));
        Ok(())
    }
    fn set_handler(&self, handler: Arc<dyn Handler>) {
        *self.handler.lock() = Some(handler);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node that pings its peer every 100 ms and counts pongs; the count
    /// survives a crash because it is written to durable state.
    struct Pinger {
        me: NodeId,
        peer: NodeId,
        seen: u64,
    }

    impl NodeLogic for Pinger {
        fn handle(
            &mut self,
            input: Input,
            clock: &dyn Clock,
            durable: &mut Durable,
        ) -> Vec<Output> {
            match input {
                Input::Start => {
                    self.seen = durable.entries.get("pongs").map(|b| b[0] as u64).unwrap_or(0);
                    vec![
                        Output::Timer { id: 1, after: 100 },
                        Output::Note(format!("started at {}", clock.now())),
                    ]
                }
                Input::Timer(1) => vec![
                    Output::Send {
                        to: self.peer.clone(),
                        envelope: Envelope::request("ping", self.me.clone(), clock.now(), vec![]),
                    },
                    Output::Timer { id: 1, after: 100 },
                ],
                Input::Timer(_) | Input::Peer(_) | Input::ShardDone { .. } => vec![],
                Input::Message(e) if e.kind == super::super::transport::Kind::Request => {
                    vec![Output::Send {
                        to: e.from.clone(),
                        envelope: e.response(self.me.clone(), vec![]),
                    }]
                }
                Input::Message(e) => {
                    self.seen += 1;
                    durable.entries.insert("pongs".into(), vec![self.seen as u8]);
                    vec![Output::Note(format!("pong {} from {}", self.seen, e.from))]
                }
            }
        }
    }

    fn pair(seed: u64) -> (Sim, NodeId, NodeId) {
        let a = NodeId("a".into());
        let b = NodeId("b".into());
        let mut sim = Sim::new(seed);
        let (pa, pb) = (b.clone(), a.clone());
        sim.add_node(
            a.clone(),
            Box::new(move |me| Box::new(Pinger { me, peer: pa.clone(), seen: 0 })),
        );
        sim.add_node(
            b.clone(),
            Box::new(move |me| Box::new(Pinger { me, peer: pb.clone(), seen: 0 })),
        );
        (sim, a, b)
    }

    #[test]
    fn pings_come_back_in_order_and_time_moves_only_by_events() {
        let (mut sim, a, _) = pair(1);
        sim.run_until(1000);
        let pongs: Vec<_> =
            sim.notes.iter().filter(|(_, n, t)| *n == a && t.starts_with("pong")).collect();
        // the first ping goes at 100 ms and each round trip takes a few, so
        // nine have come back by the thousandth millisecond
        assert_eq!(pongs.len(), 9, "{:?}", sim.notes);
        assert!(pongs.windows(2).all(|w| w[0].0 <= w[1].0));
        assert_eq!(sim.now(), 1000);
    }

    #[test]
    fn a_partition_loses_messages_and_healing_brings_them_back() {
        let (mut sim, a, b) = pair(2);
        sim.partition(&[a.clone()], &[b.clone()]);
        sim.run_until(1000);
        assert!(sim.notes.iter().all(|(_, _, t)| !t.starts_with("pong")));
        let (_, dropped) = sim.stats();
        assert!(dropped >= 18, "dropped {dropped}");
        sim.heal();
        sim.run_until(2000);
        assert!(sim.notes.iter().any(|(_, _, t)| t.starts_with("pong")));
    }

    #[test]
    fn the_same_seed_makes_the_same_run() {
        let (mut x, _, _) = pair(7);
        let (mut y, _, _) = pair(7);
        x.network.drop_rate = 0.3;
        y.network.drop_rate = 0.3;
        x.run_until(3000);
        y.run_until(3000);
        assert_eq!(x.trace, y.trace);
        let (mut z, _, _) = pair(8);
        z.network.drop_rate = 0.3;
        z.run_until(3000);
        assert_ne!(x.trace, z.trace);
    }

    #[test]
    fn a_crash_loses_timers_but_not_what_was_written() {
        let (mut sim, a, b) = pair(3);
        sim.run_until(550);
        let before = sim.durable(&a).unwrap().entries.get("pongs").cloned();
        assert!(before.is_some());
        sim.crash(&a);
        sim.run_until(1000);
        // nothing from a while it is down
        assert!(!sim.notes.iter().any(|(t, n, _)| *n == a && *t > 550 && *t <= 1000));
        assert_eq!(sim.durable(&a).unwrap().entries.get("pongs"), before.as_ref());
        sim.restart(&a);
        sim.run_until(2000);
        let after = sim.durable(&a).unwrap().entries.get("pongs").unwrap()[0];
        assert!(after > before.unwrap()[0]);
        assert!(sim.is_up(&b));
    }

    #[test]
    fn skew_moves_one_clock_only() {
        let (mut sim, a, b) = pair(4);
        sim.skew(&a, 5_000);
        sim.run_until(100);
        let started: Vec<_> =
            sim.notes.iter().filter(|(_, _, t)| t.starts_with("started")).collect();
        assert_eq!(started.len(), 2);
        // the note was made at start, before the skew; the timers after it
        // read the skewed clock
        sim.run_until(300);
        let a_pings = sim.trace.iter().filter(|l| l.contains("note a pong")).count();
        let b_pings = sim.trace.iter().filter(|l| l.contains("note b pong")).count();
        assert!(a_pings > 0 && b_pings > 0);
        assert_eq!(sim.nodes[&a].clock.now(), sim.now() + 5_000);
        assert_eq!(sim.nodes[&b].clock.now(), sim.now());
    }
}
