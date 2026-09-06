//! The vectors an index holds, beside the index rather than inside it.
//!
//! One table per index, keyed by field path, holding a vector per document.
//! The writer keeps it up to date as documents arrive and leave; a search
//! reads it. It is written to disk beside the index so that a restart does
//! not have to read every document back to learn what it already knew, and
//! rebuilt from the documents when that file is missing or stale.

use std::collections::HashMap;

use serde_json::Value;

use super::hnsw::{Hnsw, Parameters};
use super::{Field, Space};

/// The vectors one field holds, and the graph over them.
///
/// The vectors are kept by document id, and again in insertion order so that
/// the graph can name them by number -- a graph of a hundred million nodes
/// cannot afford a string per edge.
#[derive(Default)]
struct Held {
    by_id: HashMap<String, Vec<f32>>,
    /// item number to document id, which is what the graph returns
    order: Vec<String>,
    at: HashMap<String, u32>,
    graph: Option<Hnsw>,
}

/// How many vectors a field must hold before a graph is worth building.
///
/// Below this, comparing everything is both exact and faster: walking a graph
/// costs pointer-chasing and bookkeeping that only pays back once there is
/// enough to skip.
const GRAPH_ABOVE: usize = 1_000;

/// When a graph is more tombstones than vectors it is walking through work it
/// cannot use, and is cheaper to build again than to keep.
const REBUILD_WHEN_STALE: f64 = 0.5;

impl Held {
    fn put(&mut self, id: &str, vector: Vec<f32>) {
        // a document written again keeps its number, so the graph does not
        // grow a node per rewrite
        let item = match self.at.get(id) {
            Some(item) => {
                // the graph's edges were built for the old vector; the node
                // is retired and the new vector takes a number of its own
                if let Some(graph) = self.graph.as_mut() {
                    graph.remove(*item);
                }
                let item = self.order.len() as u32;
                self.order.push(id.to_string());
                self.at.insert(id.to_string(), item);
                item
            }
            None => {
                let item = self.order.len() as u32;
                self.order.push(id.to_string());
                self.at.insert(id.to_string(), item);
                item
            }
        };
        self.by_id.insert(id.to_string(), vector);
        if let Some(graph) = self.graph.as_mut() {
            let vectors = Vectors::reader(&self.by_id, &self.order);
            graph.insert(item, &vectors);
        }
    }

    fn forget(&mut self, id: &str) -> bool {
        let gone = self.by_id.remove(id).is_some();
        if let (Some(item), Some(graph)) = (self.at.remove(id), self.graph.as_mut()) {
            graph.remove(item);
        }
        gone
    }

    /// Build the graph, or throw it away and build it again.
    fn build_graph(&mut self, space: Space, parameters: Parameters) {
        let mut graph = Hnsw::new(space, parameters);
        // the order is compacted first: a graph built over retired numbers
        // would carry them for the rest of its life
        self.order.retain(|id| self.by_id.contains_key(id));
        self.order.dedup();
        self.at = self.order.iter().enumerate().map(|(i, id)| (id.clone(), i as u32)).collect();
        let vectors = Vectors::reader(&self.by_id, &self.order);
        for item in 0..self.order.len() as u32 {
            graph.insert(item, &vectors);
        }
        self.graph = Some(graph);
    }

    /// Whether the graph should be used, built, or rebuilt before a search.
    fn ready(&mut self, space: Space, parameters: Parameters) -> bool {
        let held = self.by_id.len();
        if held < GRAPH_ABOVE {
            return false;
        }
        match &self.graph {
            None => self.build_graph(space, parameters),
            Some(graph) => {
                let stale = graph.tombstones() as f64 / (graph.len().max(1)) as f64;
                if stale > REBUILD_WHEN_STALE {
                    self.build_graph(space, parameters);
                }
            }
        }
        true
    }
}

/// Every vector an index holds, by field and then by document id.
#[derive(Default)]
pub struct Vectors {
    by_field: HashMap<String, Held>,
    /// whether anything has changed since it was last written down
    dirty: bool,
}

/// One answer: which document, how far, and what that is worth.
pub struct Near {
    pub id: String,
    pub distance: f32,
    pub score: f32,
}

impl Vectors {
    /// How the graph reads a vector: by item number, through the order.
    fn reader<'a>(
        by_id: &'a HashMap<String, Vec<f32>>,
        order: &'a [String],
    ) -> impl Fn(u32) -> Option<&'a [f32]> + 'a {
        move |item: u32| by_id.get(order.get(item as usize)?).map(|v| v.as_slice())
    }

    /// Take the vectors a document carries, for the fields declared as
    /// holding them.
    ///
    /// A document that carries nothing for a field is not an error and not an
    /// entry: it simply cannot be found by a search of that field.
    pub fn write(&mut self, fields: &HashMap<String, Field>, id: &str, source: &Value) {
        for (path, field) in fields {
            match super::vector_at(source, path) {
                Some(vector) if vector.len() == field.dimension => {
                    self.by_field.entry(path.clone()).or_default().put(id, vector);
                    self.dirty = true;
                }
                _ => {
                    // a document rewritten without the field no longer has one
                    if let Some(held) = self.by_field.get_mut(path)
                        && held.forget(id)
                    {
                        self.dirty = true;
                    }
                }
            }
        }
    }

    /// Forget a document, whatever it held.
    pub fn forget(&mut self, id: &str) {
        for held in self.by_field.values_mut() {
            if held.forget(id) {
                self.dirty = true;
            }
        }
    }

    pub fn clear(&mut self) {
        self.by_field.clear();
        self.dirty = true;
    }

    /// How many vectors are held, over every field.
    pub fn len(&self) -> usize {
        self.by_field.values().map(|f| f.by_id.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether this field has anything at all.
    pub fn holds(&self, path: &str) -> bool {
        self.by_field.get(path).map(|f| !f.by_id.is_empty()).unwrap_or(false)
    }

    /// Whether a graph is standing over this field, which is what `_knn/stats`
    /// reports and what says whether a search will be approximate.
    pub fn graphed(&self, path: &str) -> bool {
        self.by_field.get(path).map(|f| f.graph.is_some()).unwrap_or(false)
    }

    /// Build or rebuild the graphs, for the fields big enough to want one.
    ///
    /// Done where an index is made durable rather than during a search: a
    /// search that had to build a graph would hold every other search out
    /// while it did, and the first one after a restart would pay for all of
    /// them.
    pub fn maintain(&mut self, fields: &HashMap<String, Field>) {
        for (path, field) in fields {
            if let Some(held) = self.by_field.get_mut(path) {
                held.ready(field.space, field.parameters);
            }
        }
    }

    /// The `k` documents nearest this vector, nearest first.
    ///
    /// Over a small field, or a filter narrow enough that walking a graph
    /// would step past most of what it is allowed to return, everything is
    /// compared: exact, and faster than a graph that has to refuse most of
    /// what it finds. Otherwise the graph answers, which is approximate and
    /// does not read the whole field.
    pub fn nearest(
        &self,
        path: &str,
        space: Space,
        asked: &[f32],
        k: usize,
        allowed: Option<&dyn Fn(&str) -> bool>,
    ) -> Vec<Near> {
        let narrow = self.filter_is_narrow(path, allowed);
        let Some(field) = self.by_field.get(path) else { return Vec::new() };
        let Some(graph) = field.graph.as_ref().filter(|_| !narrow) else {
            return self.exact(path, space, asked, Some(k), None, allowed);
        };
        let vectors = Vectors::reader(&field.by_id, &field.order);
        let keep = allowed.map(|keep| {
            move |item: u32| field.order.get(item as usize).map(|id| keep(id)).unwrap_or(false)
        });
        let keep_ref: Option<&dyn Fn(u32) -> bool> =
            keep.as_ref().map(|f| f as &dyn Fn(u32) -> bool);
        graph
            .nearest(asked, k, None, &vectors, keep_ref)
            .into_iter()
            .filter_map(|(item, distance)| {
                let id = field.order.get(item as usize)?.clone();
                Some(Near { id, distance, score: space.score(distance) })
            })
            .collect()
    }

    /// Whether a filter keeps so little that comparing everything it keeps is
    /// cheaper than walking a graph past everything it does not.
    fn filter_is_narrow(&self, path: &str, allowed: Option<&dyn Fn(&str) -> bool>) -> bool {
        let Some(keep) = allowed else { return false };
        let Some(field) = self.by_field.get(path) else { return true };
        if field.by_id.len() < GRAPH_ABOVE {
            return true;
        }
        // counting the whole field would cost what the search costs, so a
        // sample says whether the filter is narrow
        let sample: Vec<&String> = field.by_id.keys().take(512).collect();
        let kept = sample.iter().filter(|id| keep(id)).count();
        (kept as f64 / sample.len().max(1) as f64) < 0.1
    }

    /// Everything within a distance of this vector, nearest first.
    pub fn within(
        &self,
        path: &str,
        space: Space,
        asked: &[f32],
        limit: f32,
        allowed: Option<&dyn Fn(&str) -> bool>,
    ) -> Vec<Near> {
        self.exact(path, space, asked, None, Some(limit), allowed)
    }

    /// Compare against everything, which is what "exact" means.
    fn exact(
        &self,
        path: &str,
        space: Space,
        asked: &[f32],
        k: Option<usize>,
        limit: Option<f32>,
        allowed: Option<&dyn Fn(&str) -> bool>,
    ) -> Vec<Near> {
        let Some(field) = self.by_field.get(path) else { return Vec::new() };
        let mut found: Vec<Near> = field
            .by_id
            .iter()
            .filter(|(id, _)| allowed.map(|keep| keep(id)).unwrap_or(true))
            .map(|(id, vector)| {
                let distance = space.distance(asked, vector);
                Near { id: id.clone(), distance, score: space.score(distance) }
            })
            .filter(|near| {
                near.distance.is_finite() && limit.map(|l| near.distance <= l).unwrap_or(true)
            })
            .collect();
        found.sort_by(|a, b| a.distance.total_cmp(&b.distance));
        if let Some(k) = k {
            found.truncate(k);
        }
        found
    }

    /// The vector one document holds, for a script that wants to do its own
    /// arithmetic with it.
    pub fn get(&self, path: &str, id: &str) -> Option<&[f32]> {
        self.by_field.get(path)?.by_id.get(id).map(|v| v.as_slice())
    }

    /// Write the table down, as it stands.
    pub fn save(&mut self, path: &std::path::Path) {
        if !self.dirty {
            return;
        }
        let mut out = Vec::new();
        // a plain format, written by hand: field, id, then the numbers. A
        // hundred thousand vectors of a thousand dimensions is four hundred
        // megabytes, and JSON would make it three times that. The graph is
        // not written: it is built from these, and building it again costs
        // less than keeping it correct on disk would.
        for (field, held) in &self.by_field {
            for (id, vector) in &held.by_id {
                out.extend_from_slice(&(field.len() as u32).to_le_bytes());
                out.extend_from_slice(field.as_bytes());
                out.extend_from_slice(&(id.len() as u32).to_le_bytes());
                out.extend_from_slice(id.as_bytes());
                out.extend_from_slice(&(vector.len() as u32).to_le_bytes());
                for value in vector {
                    out.extend_from_slice(&value.to_le_bytes());
                }
            }
        }
        if std::fs::write(path, out).is_ok() {
            self.dirty = false;
        }
    }

    /// Read a table back.
    pub fn load(path: &std::path::Path) -> Option<Vectors> {
        let bytes = std::fs::read(path).ok()?;
        let mut at = 0usize;
        let mut out = Vectors::default();
        let take = |at: &mut usize, n: usize| -> Option<&[u8]> {
            let found = bytes.get(*at..*at + n)?;
            *at += n;
            Some(found)
        };
        let count = |at: &mut usize| -> Option<usize> {
            let raw = take(at, 4)?;
            Some(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize)
        };
        while at < bytes.len() {
            let field =
                String::from_utf8(count(&mut at).and_then(|n| take(&mut at, n))?.to_vec()).ok()?;
            let id =
                String::from_utf8(count(&mut at).and_then(|n| take(&mut at, n))?.to_vec()).ok()?;
            let dimension = count(&mut at)?;
            let raw = take(&mut at, dimension * 4)?;
            let vector: Vec<f32> =
                raw.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
            out.by_field.entry(field).or_default().put(&id, vector);
        }
        out.dirty = false;
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn one_field() -> HashMap<String, Field> {
        HashMap::from([(
            "embedding".to_string(),
            Field {
                path: "embedding".into(),
                dimension: 2,
                space: Space::L2,
                parameters: Parameters::default(),
            },
        )])
    }

    #[test]
    fn the_nearest_is_the_one_asked_for() {
        let fields = one_field();
        let mut held = Vectors::default();
        held.write(&fields, "near", &json!({"embedding": [1.0, 0.0]}));
        held.write(&fields, "far", &json!({"embedding": [0.0, 9.0]}));
        let found = held.nearest("embedding", Space::L2, &[1.0, 0.1], 1, None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "near");
    }

    #[test]
    fn a_document_rewritten_without_the_field_is_forgotten() {
        let fields = one_field();
        let mut held = Vectors::default();
        held.write(&fields, "one", &json!({"embedding": [1.0, 0.0]}));
        assert!(held.holds("embedding"));
        held.write(&fields, "one", &json!({"something_else": 3}));
        assert!(!held.holds("embedding"));
    }

    #[test]
    fn a_vector_of_the_wrong_length_is_not_kept() {
        let fields = one_field();
        let mut held = Vectors::default();
        held.write(&fields, "one", &json!({"embedding": [1.0, 2.0, 3.0]}));
        assert!(held.is_empty());
    }

    #[test]
    fn a_table_survives_being_written_down() {
        let fields = one_field();
        let mut held = Vectors::default();
        held.write(&fields, "one", &json!({"embedding": [1.5, -2.5]}));
        held.write(&fields, "two", &json!({"embedding": [0.0, 0.0]}));
        let path = std::env::temp_dir().join(format!("boost-vectors-{}", std::process::id()));
        held.save(&path);
        let read = Vectors::load(&path).expect("the table reads back");
        assert_eq!(read.len(), 2);
        assert_eq!(read.get("embedding", "one"), Some([1.5f32, -2.5].as_slice()));
        let _ = std::fs::remove_file(&path);
    }
}
