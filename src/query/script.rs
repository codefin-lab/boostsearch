//! A `script` query: a script says, of each document, whether it matches.
//!
//! There is no index to read for this -- every document in a segment is
//! opened and the script run over it, so a `script` clause costs a scan.

use super::*;
use crate::painless::contexts::run_on_doc;
use boostcore::query::{BitSetDocSet, ConstScorer, Explanation, Scorer};
use boostcore::{DocId, Score, SegmentReader};

pub struct ScriptQuery {
    pub spec: Value,
    pub mapping: Mapping,
    pub fields: Fields,
}

impl std::fmt::Debug for ScriptQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ScriptQuery({})", self.spec)
    }
}

impl Clone for ScriptQuery {
    fn clone(&self) -> Self {
        ScriptQuery { spec: self.spec.clone(), mapping: self.mapping.clone(), fields: self.fields }
    }
}

impl Query for ScriptQuery {
    fn weight(&self, _enable_scoring: EnableScoring<'_>) -> boostcore::Result<Box<dyn Weight>> {
        Ok(Box::new(self.clone()))
    }
}

impl ScriptQuery {
    /// Every live document of the segment the script answers true for.
    fn matching(&self, reader: &SegmentReader) -> boostcore::Result<boostcore_common::BitSet> {
        let mut bits = boostcore_common::BitSet::with_max_value(reader.max_doc());
        let store = reader.get_store_reader(1)?;
        for doc in 0..reader.max_doc() {
            if reader.is_deleted(doc) {
                continue;
            }
            let held: TantivyDocument = store.get(doc)?;
            let Some(raw) = held.get_first(self.fields.source).and_then(|v| v.as_str()) else {
                continue;
            };
            let Ok(source) = serde_json::from_str::<Value>(raw) else { continue };
            let expanded = crate::store::expand_for_indexing(source, &self.mapping);
            match run_on_doc(&self.spec, &expanded, &self.mapping, 0.0) {
                Ok(v) if v.truthy() == Some(true) => bits.insert(doc),
                Ok(_) => {}
                Err(e) => {
                    return Err(TantivyError::InvalidArgument(format!(
                        "script_exception:{}",
                        e.to_json()
                    )));
                }
            }
        }
        Ok(bits)
    }
}

impl Weight for ScriptQuery {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> boostcore::Result<Box<dyn Scorer>> {
        let bits = self.matching(reader)?;
        Ok(Box::new(ConstScorer::new(BitSetDocSet::from(bits), boost)))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> boostcore::Result<Explanation> {
        let bits = self.matching(reader)?;
        if bits.contains(doc) {
            Ok(Explanation::new("ScriptQuery", 1.0))
        } else {
            Err(TantivyError::InvalidArgument("does not match".into()))
        }
    }

    fn count(&self, reader: &SegmentReader) -> boostcore::Result<u32> {
        Ok(self.matching(reader)?.len() as u32)
    }
}
