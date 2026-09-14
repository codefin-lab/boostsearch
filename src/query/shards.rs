//! The documents of some of an index's shards.
//!
//! One BoostCore index holds every shard of an OpenSearch index, so a search
//! narrowed by `routing` or `preference=_shards:` cannot be sent to fewer
//! indices: it is narrowed to the documents those shards hold instead. Where
//! a document lives is worked out as a write placed it -- from the routing
//! kept in the document, or from its id where it has none.

use super::*;
use boostcore::query::{BitSetDocSet, ConstScorer, Explanation, Scorer};
use boostcore::{DocId, Score, SegmentReader};

/// How an index folds a routing value into a shard, and which shards are
/// wanted. Written into the request as `_bs_on_shards` by the search that
/// narrows, and read back here.
#[derive(Clone, Debug)]
pub struct OnShards {
    pub shards: Vec<u64>,
    pub of: u64,
    pub routing_shards: Option<u64>,
    pub partition: u64,
}

impl OnShards {
    pub fn from_json(body: &Value) -> Result<OnShards> {
        let shards = body
            .get("shards")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_u64()).collect())
            .ok_or_else(|| anyhow!("[_bs_on_shards] needs [shards]"))?;
        Ok(OnShards {
            shards,
            of: body.get("of").and_then(|v| v.as_u64()).unwrap_or(1).max(1),
            routing_shards: body.get("routing_shards").and_then(|v| v.as_u64()),
            partition: body.get("partition").and_then(|v| v.as_u64()).unwrap_or(1).max(1),
        })
    }

    /// The shard a document with this id and routing is on -- the same sum
    /// `IdxState::shard_of` does, from what the request carried.
    fn shard_of(&self, id: &str, routing: Option<&str>) -> u64 {
        match routing {
            None => crate::search::routing_shard_in(id, self.of, self.routing_shards),
            Some(r) => {
                let offset = match self.partition {
                    1 => 0,
                    size => (crate::search::routing_hash(id) as i64).rem_euclid(size as i64) as i32,
                };
                crate::search::routing_shard_offset(r, offset, self.of, self.routing_shards)
            }
        }
    }

    fn matching(&self, reader: &SegmentReader) -> boostcore::Result<boostcore_common::BitSet> {
        let mut bits = boostcore_common::BitSet::with_max_value(reader.max_doc());
        let ff = reader.fast_fields();
        let Ok(Some(ids)) = ff.str("_id") else { return Ok(bits) };
        let column = format!("{}.{}", crate::store::RAW, crate::store::ROUTING_KEY);
        let routings = ff.str(&column).ok().flatten();
        let (mut id, mut routing) = (String::new(), String::new());
        for doc in 0..reader.max_doc() {
            if reader.is_deleted(doc) {
                continue;
            }
            let Some(ord) = ids.term_ords(doc).next() else { continue };
            id.clear();
            if ids.ord_to_str(ord, &mut id).is_err() {
                continue;
            }
            routing.clear();
            let routed = routings
                .as_ref()
                .and_then(|col| col.term_ords(doc).next().map(|o| (col, o)))
                .map(|(col, o)| col.ord_to_str(o, &mut routing).is_ok())
                .unwrap_or(false);
            let shard = self.shard_of(&id, routed.then_some(routing.as_str()));
            if self.shards.contains(&shard) {
                bits.insert(doc);
            }
        }
        Ok(bits)
    }
}

impl Query for OnShards {
    fn weight(&self, _enable_scoring: EnableScoring<'_>) -> boostcore::Result<Box<dyn Weight>> {
        Ok(Box::new(self.clone()))
    }
}

impl Weight for OnShards {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> boostcore::Result<Box<dyn Scorer>> {
        let bits = self.matching(reader)?;
        Ok(Box::new(ConstScorer::new(BitSetDocSet::from(bits), boost)))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> boostcore::Result<Explanation> {
        if self.matching(reader)?.contains(doc) {
            Ok(Explanation::new("OnShards", 0.0))
        } else {
            Err(TantivyError::InvalidArgument("does not match".into()))
        }
    }

    fn count(&self, reader: &SegmentReader) -> boostcore::Result<u32> {
        Ok(self.matching(reader)?.len() as u32)
    }
}
