//! The vectors an index holds, beside the index rather than inside it.
//!
//! One table per index, keyed by field path, holding a vector per document.
//! The writer keeps it up to date as documents arrive and leave; a search
//! reads it. It is written to disk beside the index so that a restart does
//! not have to read every document back to learn what it already knew, and
//! rebuilt from the documents when that file is missing or stale.

use std::collections::HashMap;

use serde_json::Value;

use super::{Field, Space};

/// Every vector an index holds, by field and then by document id.
#[derive(Default)]
pub struct Vectors {
    by_field: HashMap<String, HashMap<String, Vec<f32>>>,
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
    /// Take the vectors a document carries, for the fields declared as
    /// holding them.
    ///
    /// A document that carries nothing for a field is not an error and not an
    /// entry: it simply cannot be found by a search of that field.
    pub fn write(&mut self, fields: &HashMap<String, Field>, id: &str, source: &Value) {
        for (path, field) in fields {
            match super::vector_at(source, path) {
                Some(vector) if vector.len() == field.dimension => {
                    self.by_field.entry(path.clone()).or_default().insert(id.to_string(), vector);
                    self.dirty = true;
                }
                _ => {
                    // a document rewritten without the field no longer has one
                    if let Some(held) = self.by_field.get_mut(path)
                        && held.remove(id).is_some()
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
            if held.remove(id).is_some() {
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
        self.by_field.values().map(|f| f.len()) .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether this field has anything at all, which is what tells a search
    /// that the table needs building before it can be read.
    pub fn holds(&self, path: &str) -> bool {
        self.by_field.get(path).map(|f| !f.is_empty()).unwrap_or(false)
    }

    /// The `k` documents nearest this vector, nearest first.
    ///
    /// Every vector is compared. An approximate index answers faster and can
    /// be wrong; this is exact, which is what makes it the thing to measure an
    /// approximate one against.
    pub fn nearest(
        &self,
        path: &str,
        space: Space,
        asked: &[f32],
        k: usize,
        allowed: Option<&dyn Fn(&str) -> bool>,
    ) -> Vec<Near> {
        let Some(held) = self.by_field.get(path) else { return Vec::new() };
        let mut found: Vec<Near> = held
            .iter()
            .filter(|(id, _)| allowed.map(|keep| keep(id)).unwrap_or(true))
            .map(|(id, vector)| {
                let distance = space.distance(asked, vector);
                Near { id: id.clone(), distance, score: space.score(distance) }
            })
            .filter(|near| near.distance.is_finite())
            .collect();
        found.sort_by(|a, b| a.distance.total_cmp(&b.distance));
        found.truncate(k);
        found
    }

    /// Everything within a distance of this vector, nearest first.
    ///
    /// This is what a radial search asks for: not the nearest few, but
    /// everything close enough, however many that turns out to be.
    pub fn within(
        &self,
        path: &str,
        space: Space,
        asked: &[f32],
        limit: f32,
        allowed: Option<&dyn Fn(&str) -> bool>,
    ) -> Vec<Near> {
        let Some(held) = self.by_field.get(path) else { return Vec::new() };
        let mut found: Vec<Near> = held
            .iter()
            .filter(|(id, _)| allowed.map(|keep| keep(id)).unwrap_or(true))
            .map(|(id, vector)| {
                let distance = space.distance(asked, vector);
                Near { id: id.clone(), distance, score: space.score(distance) }
            })
            .filter(|near| near.distance.is_finite() && near.distance <= limit)
            .collect();
        found.sort_by(|a, b| a.distance.total_cmp(&b.distance));
        found
    }

    /// The vector one document holds, for a script that wants to do its own
    /// arithmetic with it.
    pub fn get(&self, path: &str, id: &str) -> Option<&[f32]> {
        self.by_field.get(path)?.get(id).map(|v| v.as_slice())
    }

    /// Write the table down, as it stands.
    pub fn save(&mut self, path: &std::path::Path) {
        if !self.dirty {
            return;
        }
        let mut out = Vec::new();
        // a plain format, written by hand: field, id, then the numbers. A
        // hundred thousand vectors of a thousand dimensions is four hundred
        // megabytes, and JSON would make it three times that.
        for (field, held) in &self.by_field {
            for (id, vector) in held {
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
            let field = String::from_utf8(count(&mut at).and_then(|n| take(&mut at, n))?.to_vec())
                .ok()?;
            let id =
                String::from_utf8(count(&mut at).and_then(|n| take(&mut at, n))?.to_vec()).ok()?;
            let dimension = count(&mut at)?;
            let raw = take(&mut at, dimension * 4)?;
            let vector: Vec<f32> = raw
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();
            out.by_field.entry(field).or_default().insert(id, vector);
        }
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
            Field { path: "embedding".into(), dimension: 2, space: Space::L2 },
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
