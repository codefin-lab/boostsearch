//! Vector search: finding the documents nearest a point rather than the ones
//! holding a word.
//!
//! A `knn_vector` field holds an array of numbers -- what a model made of a
//! sentence, a picture, a face. Searching it means finding the documents
//! whose arrays are closest to the one asked about, under some idea of close.
//!
//! Vectors do not live in the inverted index. A term dictionary answers "which
//! documents hold this word", and no arrangement of it answers "which
//! documents are near this point": the question is about distance in three
//! hundred dimensions, not about equality. So they live beside it, in a table
//! of their own that the writer keeps up to date and that a search reads.

use std::collections::HashMap;

use serde_json::Value;

pub mod space;
pub mod store;

pub use space::Space;
pub use store::Vectors;

/// What a `knn_vector` field was declared as.
#[derive(Clone, Debug)]
pub struct Field {
    pub path: String,
    pub dimension: usize,
    pub space: Space,
}

/// Every `knn_vector` field a mapping declares, by path.
///
/// Worked out when the mapping changes rather than per document: a write has
/// to ask this of every field it is given, and most mappings have none at all.
pub fn fields_of(mapping: &Value) -> HashMap<String, Field> {
    let mut out = HashMap::new();
    walk(mapping.get("properties"), "", &mut out);
    out
}

fn walk(properties: Option<&Value>, prefix: &str, out: &mut HashMap<String, Field>) {
    let Some(properties) = properties.and_then(|p| p.as_object()) else { return };
    for (name, spec) in properties {
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        match spec.get("type").and_then(|t| t.as_str()) {
            Some("knn_vector") => {
                let Some(dimension) = spec.get("dimension").and_then(|v| v.as_u64()) else {
                    continue;
                };
                out.insert(
                    path.clone(),
                    Field {
                        path,
                        dimension: dimension as usize,
                        // the space is named on the method, and l2 is what a
                        // field that does not name one is measured in
                        space: Space::named(
                            spec.pointer("/method/space_type")
                                .or_else(|| spec.get("space_type"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("l2"),
                        ),
                    },
                );
            }
            // an object or a nested object may hold one further down
            _ => walk(spec.get("properties"), &path, out),
        }
    }
}

/// Read a vector out of a document, wherever in it the field sits.
pub fn vector_at(source: &Value, path: &str) -> Option<Vec<f32>> {
    let mut here = source;
    for part in path.split('.') {
        here = here.get(part)?;
    }
    as_vector(here)
}

/// A vector as it may be written: an array of numbers.
pub fn as_vector(value: &Value) -> Option<Vec<f32>> {
    let items = value.as_array()?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(item.as_f64()? as f32);
    }
    (!out.is_empty()).then_some(out)
}
