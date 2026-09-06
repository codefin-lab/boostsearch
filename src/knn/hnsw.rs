//! A navigable small-world graph, in layers.
//!
//! Comparing a query against every vector is exact and costs a pass over the
//! whole collection. This is the other way: every vector is a node in a graph
//! whose edges join it to a few of its neighbours, and a search walks that
//! graph downhill towards the query instead of reading everything.
//!
//! The graph is built in layers. Most nodes live only in the bottom one,
//! which holds everything; each layer above holds a fraction of the one
//! below, so a search can cross the whole collection in a few steps up top
//! and then refine downwards. That is the whole idea: the top layers are a
//! coarse map, the bottom layer is the detail.
//!
//! It is approximate. A search can walk into a valley and miss a nearer
//! vector on the other side of a ridge, which is why `ef` exists -- the
//! number of candidates kept in play while walking. Larger is slower and
//! closer to exact, and the tests here measure that rather than assuming it.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};

use super::Space;

/// How many neighbours a node keeps, and how hard the build and the search
/// look.
///
/// The defaults are measured rather than copied. Over fifty thousand vectors
/// of sixty-four dimensions, against the exact answer:
///
///     ef_construction  ef_search   build    query   recall
///                 100        100   18.6s   0.89ms    0.805
///                 200        200   30.8s   0.84ms    0.950
///                 512        256   54.3s   1.08ms    0.970
///
/// A recall of 0.8 is not a default anybody should be given: one search in
/// five missing a document it should have found is the kind of wrongness
/// nobody notices until it matters. 200 buys 0.95 for the same query time and
/// half again the build. A mapping that wants OpenSearch's own 512 can ask
/// for it, and pay for it.
#[derive(Clone, Copy, Debug)]
pub struct Parameters {
    /// neighbours per node on the upper layers; the bottom layer keeps twice
    pub m: usize,
    /// how many candidates are held while inserting
    pub ef_construction: usize,
    /// how many are held while searching, unless a search says otherwise
    pub ef_search: usize,
}

impl Default for Parameters {
    fn default() -> Self {
        Parameters { m: 16, ef_construction: 200, ef_search: 200 }
    }
}

struct Node {
    /// which document this is, as an index into the caller's own list
    item: u32,
    /// the neighbours of this node, one list per layer it appears in
    links: Vec<Vec<u32>>,
}

/// A graph over a set of vectors.
pub struct Hnsw {
    parameters: Parameters,
    space: Space,
    nodes: Vec<Node>,
    /// where a search starts: the node in the highest layer
    entry: Option<u32>,
    /// nodes whose documents have gone; still walked through, never returned
    removed: HashSet<u32>,
}

/// A candidate, ordered so that the *furthest* is on top of a max-heap --
/// which is what lets a bounded search throw away the worst of what it holds.
#[derive(PartialEq)]
struct Candidate {
    distance: f32,
    node: u32,
}

impl Eq for Candidate {}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.distance.total_cmp(&other.distance)
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The same, ordered the other way, for the queue of places still to try.
#[derive(PartialEq)]
struct Nearest(Candidate);

impl Eq for Nearest {}

impl Ord for Nearest {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.distance.total_cmp(&self.0.distance)
    }
}

impl PartialOrd for Nearest {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Hnsw {
    pub fn new(space: Space, parameters: Parameters) -> Hnsw {
        Hnsw { parameters, space, nodes: Vec::new(), entry: None, removed: HashSet::new() }
    }

    pub fn len(&self) -> usize {
        self.nodes.len() - self.removed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many nodes are in the graph but no longer wanted.
    ///
    /// A graph that is mostly tombstones is walking through work it cannot
    /// use, and is cheaper to build again than to keep.
    pub fn tombstones(&self) -> usize {
        self.removed.len()
    }

    /// Which layer a new node goes up to.
    ///
    /// The level is drawn from an exponential distribution, so each layer
    /// holds about `1/m` of the one below it. It is drawn from the node's own
    /// number rather than from a random source, so that the same vectors
    /// inserted in the same order always give the same graph -- a search that
    /// misses something should be reproducible.
    fn level_for(&self, node: u32) -> usize {
        // a cheap deterministic hash, spread over the whole of u64
        let mut x = node as u64 ^ 0x9E37_79B9_7F4A_7C15;
        x ^= x >> 30;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        let unit = (x >> 11) as f64 / (1u64 << 53) as f64;
        let ml = 1.0 / (self.parameters.m as f64).ln();
        let level = (-(unit.max(f64::MIN_POSITIVE)).ln() * ml).floor();
        // a level nothing can reach is a level nothing gains from
        (level as usize).min(16)
    }

    /// Put a vector in the graph.
    ///
    /// `item` is the caller's own name for it, and `vectors` is how the graph
    /// reads any vector it needs -- borrowed, never copied: a search compares
    /// against thousands of them and a copy apiece is what would make walking
    /// a graph slower than reading everything.
    pub fn insert<'v>(&mut self, item: u32, vectors: &dyn Fn(u32) -> Option<&'v [f32]>) {
        let Some(vector) = vectors(item) else { return };
        let node = self.nodes.len() as u32;
        let level = self.level_for(node);
        self.nodes.push(Node { item, links: vec![Vec::new(); level + 1] });

        let Some(entry) = self.entry else {
            self.entry = Some(node);
            return;
        };
        let top = self.nodes[entry as usize].links.len() - 1;

        // walk down the layers above this node's own, one greedy step at a
        // time: those layers are a map, not a search
        let mut here = entry;
        for layer in (level + 1..=top).rev() {
            here = self.descend(here, vector, layer, vectors);
        }

        // and join the layers it does belong to
        for layer in (0..=level.min(top)).rev() {
            let found =
                self.search_layer(vector, &[here], layer, self.parameters.ef_construction, vectors, None);
            let wanted = if layer == 0 { self.parameters.m * 2 } else { self.parameters.m };
            let chosen = self.choose(vector, found, wanted, vectors);
            for other in &chosen {
                self.link(node, *other, layer);
                self.link(*other, node, layer);
                self.prune(*other, layer, wanted, vectors);
            }
            here = chosen.first().copied().unwrap_or(here);
        }
        if level > top {
            self.entry = Some(node);
        }
    }

    /// Mark a node's document as gone.
    pub fn remove(&mut self, item: u32) {
        if let Some(node) = self.nodes.iter().position(|n| n.item == item) {
            self.removed.insert(node as u32);
        }
    }

    /// One greedy step at a time towards the query, on one layer.
    fn descend<'v>(
        &self,
        from: u32,
        query: &[f32],
        layer: usize,
        vectors: &dyn Fn(u32) -> Option<&'v [f32]>,
    ) -> u32 {
        let mut here = from;
        let mut best = self.distance(here, query, vectors);
        loop {
            let mut moved = false;
            for next in self.links_of(here, layer).to_vec() {
                let d = self.distance(next, query, vectors);
                if d < best {
                    best = d;
                    here = next;
                    moved = true;
                }
            }
            if !moved {
                return here;
            }
        }
    }

    /// The `ef` nearest nodes to the query on one layer, starting from a set
    /// of entry points.
    ///
    /// This is the whole of the search: hold the best `ef` found so far, keep
    /// stepping to the nearest unvisited neighbour, and stop when the nearest
    /// place left to try is further away than the worst thing already held.
    fn search_layer<'v>(
        &self,
        query: &[f32],
        entries: &[u32],
        layer: usize,
        ef: usize,
        vectors: &dyn Fn(u32) -> Option<&'v [f32]>,
        keep: Option<&dyn Fn(u32) -> bool>,
    ) -> Vec<Candidate> {
        let mut seen: HashSet<u32> = entries.iter().copied().collect();
        let mut to_try: BinaryHeap<Nearest> = BinaryHeap::new();
        let mut held: BinaryHeap<Candidate> = BinaryHeap::new();
        for entry in entries {
            let distance = self.distance(*entry, query, vectors);
            to_try.push(Nearest(Candidate { distance, node: *entry }));
            // a node whose document is gone, or which a filter excludes, is
            // still walked through -- it is part of the road -- but it is not
            // an answer
            if self.wanted(*entry, keep) {
                held.push(Candidate { distance, node: *entry });
            }
        }
        while let Some(Nearest(closest)) = to_try.pop() {
            let worst = held.peek().map(|c| c.distance).unwrap_or(f32::INFINITY);
            if closest.distance > worst && held.len() >= ef {
                break;
            }
            for next in self.links_of(closest.node, layer).iter().copied() {
                if !seen.insert(next) {
                    continue;
                }
                let distance = self.distance(next, query, vectors);
                let worst = held.peek().map(|c| c.distance).unwrap_or(f32::INFINITY);
                if held.len() < ef || distance < worst {
                    to_try.push(Nearest(Candidate { distance, node: next }));
                    if self.wanted(next, keep) {
                        held.push(Candidate { distance, node: next });
                        if held.len() > ef {
                            held.pop();
                        }
                    }
                }
            }
        }
        let mut out = held.into_vec();
        out.sort_by(|a, b| a.distance.total_cmp(&b.distance));
        out
    }

    /// The `k` nearest items to a query, as `(item, distance)`.
    pub fn nearest<'v>(
        &self,
        query: &[f32],
        k: usize,
        ef: Option<usize>,
        vectors: &dyn Fn(u32) -> Option<&'v [f32]>,
        keep: Option<&dyn Fn(u32) -> bool>,
    ) -> Vec<(u32, f32)> {
        let Some(entry) = self.entry else { return Vec::new() };
        let top = self.nodes[entry as usize].links.len() - 1;
        let mut here = entry;
        for layer in (1..=top).rev() {
            here = self.descend(here, query, layer, vectors);
        }
        let ef = ef.unwrap_or(self.parameters.ef_search).max(k);
        let found = self.search_layer(query, &[here], 0, ef, vectors, keep);
        found
            .into_iter()
            .take(k)
            .map(|c| (self.nodes[c.node as usize].item, c.distance))
            .collect()
    }

    /// Which of the candidates a node should keep as neighbours.
    ///
    /// Not simply the nearest: a node whose neighbours are all in one
    /// direction is a dead end from every other direction. A candidate is
    /// kept only if it is nearer to the new node than to anything already
    /// kept, which spreads the links out and is what makes the graph
    /// navigable rather than merely well-connected.
    fn choose<'v>(
        &self,
        vector: &[f32],
        candidates: Vec<Candidate>,
        wanted: usize,
        vectors: &dyn Fn(u32) -> Option<&'v [f32]>,
    ) -> Vec<u32> {
        let mut kept: Vec<u32> = Vec::with_capacity(wanted);
        for candidate in candidates {
            if kept.len() >= wanted {
                break;
            }
            let Some(theirs) = vectors(self.nodes[candidate.node as usize].item) else { continue };
            let nearer_to_us = kept.iter().all(|other| {
                let Some(other_vector) = vectors(self.nodes[*other as usize].item) else {
                    return true;
                };
                candidate.distance < self.space.distance(theirs, other_vector)
            });
            if nearer_to_us || kept.is_empty() {
                kept.push(candidate.node);
            }
        }
        kept
    }

    /// Keep a node's links down to what it is allowed.
    fn prune<'v>(
        &mut self,
        node: u32,
        layer: usize,
        wanted: usize,
        vectors: &dyn Fn(u32) -> Option<&'v [f32]>,
    ) {
        let links = self.links_of(node, layer).to_vec();
        if links.len() <= wanted {
            return;
        }
        let Some(vector) = vectors(self.nodes[node as usize].item) else { return };
        let mut candidates: Vec<Candidate> = links
            .into_iter()
            .map(|other| Candidate { distance: self.distance(other, vector, vectors), node: other })
            .collect();
        candidates.sort_by(|a, b| a.distance.total_cmp(&b.distance));
        let kept = self.choose(vector, candidates, wanted, vectors);
        if let Some(list) = self.nodes[node as usize].links.get_mut(layer) {
            *list = kept;
        }
    }

    fn link(&mut self, from: u32, to: u32, layer: usize) {
        if from == to {
            return;
        }
        if let Some(list) = self.nodes[from as usize].links.get_mut(layer)
            && !list.contains(&to)
        {
            list.push(to);
        }
    }

    fn links_of(&self, node: u32, layer: usize) -> &[u32] {
        self.nodes[node as usize].links.get(layer).map(|l| l.as_slice()).unwrap_or(&[])
    }

    fn wanted(&self, node: u32, keep: Option<&dyn Fn(u32) -> bool>) -> bool {
        !self.removed.contains(&node)
            && keep.map(|k| k(self.nodes[node as usize].item)).unwrap_or(true)
    }

    fn distance<'v>(
        &self,
        node: u32,
        query: &[f32],
        vectors: &dyn Fn(u32) -> Option<&'v [f32]>,
    ) -> f32 {
        match vectors(self.nodes[node as usize].item) {
            Some(theirs) => self.space.distance(query, theirs),
            None => f32::INFINITY,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A spread of vectors that is not a straight line, so that a graph has
    /// somewhere to go wrong.
    fn scattered(count: usize, dimensions: usize) -> Vec<Vec<f32>> {
        let mut out = Vec::with_capacity(count);
        let mut x = 12_345u64;
        for _ in 0..count {
            let mut v = Vec::with_capacity(dimensions);
            for _ in 0..dimensions {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                v.push(((x >> 33) as f32 / (1u32 << 31) as f32) - 0.5);
            }
            out.push(v);
        }
        out
    }

    fn build(vectors: &[Vec<f32>], space: Space) -> Hnsw {
        let mut graph = Hnsw::new(space, Parameters::default());
        let read = |i: u32| vectors.get(i as usize).map(|v: &Vec<f32>| v.as_slice());
        for i in 0..vectors.len() {
            graph.insert(i as u32, &read);
        }
        graph
    }

    fn exactly(vectors: &[Vec<f32>], query: &[f32], k: usize, space: Space) -> Vec<u32> {
        let mut all: Vec<(u32, f32)> = vectors
            .iter()
            .enumerate()
            .map(|(i, v)| (i as u32, space.distance(query, v)))
            .collect();
        all.sort_by(|a, b| a.1.total_cmp(&b.1));
        all.into_iter().take(k).map(|(i, _)| i).collect()
    }

    #[test]
    fn it_finds_what_an_exact_search_finds() {
        let vectors = scattered(500, 8);
        let graph = build(&vectors, Space::L2);
        let read = |i: u32| vectors.get(i as usize).map(|v: &Vec<f32>| v.as_slice());
        let mut hits = 0;
        let queries = scattered(40, 8);
        for query in &queries {
            let want = exactly(&vectors, query, 10, Space::L2);
            let got: Vec<u32> =
                graph.nearest(query, 10, Some(100), &read, None).into_iter().map(|(i, _)| i).collect();
            hits += got.iter().filter(|i| want.contains(i)).count();
        }
        // an approximate search is allowed to miss; missing more than a tenth
        // of what an exact search finds means the graph is not being built
        let recall = hits as f64 / (queries.len() * 10) as f64;
        assert!(recall > 0.9, "recall was {recall:.3}, which is too low to trust");
    }

    #[test]
    fn the_nearest_is_the_one_asked_for() {
        let vectors = scattered(300, 6);
        let graph = build(&vectors, Space::L2);
        let read = |i: u32| vectors.get(i as usize).map(|v: &Vec<f32>| v.as_slice());
        // a vector already in the graph must find itself first
        for wanted in [0u32, 7, 42, 299] {
            let found = graph.nearest(&vectors[wanted as usize], 1, Some(64), &read, None);
            assert_eq!(found.first().map(|(i, _)| *i), Some(wanted));
        }
    }

    #[test]
    fn a_filter_is_obeyed_and_still_finds_things() {
        let vectors = scattered(400, 5);
        let graph = build(&vectors, Space::L2);
        let read = |i: u32| vectors.get(i as usize).map(|v: &Vec<f32>| v.as_slice());
        let allowed = |i: u32| i % 5 == 0;
        let found = graph.nearest(&vectors[3], 10, Some(200), &read, Some(&allowed));
        assert_eq!(found.len(), 10, "a filter that keeps a fifth should still fill ten");
        assert!(found.iter().all(|(i, _)| allowed(*i)), "everything returned passes the filter");
    }

    #[test]
    fn what_is_removed_is_not_returned() {
        let vectors = scattered(200, 4);
        let mut graph = build(&vectors, Space::L2);
        let read = |i: u32| vectors.get(i as usize).map(|v: &Vec<f32>| v.as_slice());
        graph.remove(11);
        assert_eq!(graph.tombstones(), 1);
        let found = graph.nearest(&vectors[11], 5, Some(64), &read, None);
        assert!(found.iter().all(|(i, _)| *i != 11), "a removed vector is never an answer");
        assert_eq!(found.len(), 5, "and the search still fills its answer");
    }

    #[test]
    fn a_bigger_ef_finds_at_least_as_much() {
        let vectors = scattered(600, 10);
        let graph = build(&vectors, Space::L2);
        let read = |i: u32| vectors.get(i as usize).map(|v: &Vec<f32>| v.as_slice());
        let query = &scattered(1, 10)[0];
        let want = exactly(&vectors, query, 10, Space::L2);
        let recall_at = |ef: usize| {
            let got: Vec<u32> =
                graph.nearest(query, 10, Some(ef), &read, None).into_iter().map(|(i, _)| i).collect();
            got.iter().filter(|i| want.contains(i)).count()
        };
        assert!(
            recall_at(200) >= recall_at(10),
            "looking harder should not find less: {} against {}",
            recall_at(200),
            recall_at(10)
        );
    }
}
