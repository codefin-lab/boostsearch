//! Per-block min/max for numeric fast fields, and a range query that uses them.
//!
//! BoostCore's columnar keeps min/max for a whole column, so a range that overlaps
//! the column at all still walks every value. Recording min/max per block of
//! 512 documents lets whole blocks be skipped, and lets blocks that sit entirely
//! inside the range be emitted without a single comparison. On a clustered
//! column -- a timestamp, an increasing id -- that is the difference between
//! scanning the index and touching only the part that can match.
//!
//! Stats are derived, so they are built on first use and cached per segment
//! rather than persisted: a BoostCore segment id never refers to different data,
//! which makes the cache key exact and merges self-invalidating.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use boostcore::columnar::{Column, ColumnType};
use boostcore::query::{BitSetDocSet, ConstScorer, EnableScoring, Explanation, Query, Scorer, Weight};
use boostcore::{DocId, SegmentReader, Score, TantivyError};

/// Documents per statistics block. Matches the columnar's own block size, so a
/// surviving block maps onto work the column is already organised to do.
pub const BLOCK: u32 = 512;

pub struct BlockStats {
    /// `(min, max)` per block, in the column's monotonic u64 encoding
    blocks: Vec<(u64, u64)>,
    num_docs: u32,
}

impl BlockStats {
    fn build(col: &Column<u64>, num_docs: u32) -> BlockStats {
        let mut blocks = Vec::with_capacity((num_docs / BLOCK) as usize + 1);
        let mut doc = 0u32;
        while doc < num_docs {
            let end = (doc + BLOCK).min(num_docs);
            let mut lo = u64::MAX;
            let mut hi = 0u64;
            for d in doc..end {
                // every value, not just the first: a multi-valued document must
                // not be able to hide a match inside a block we then skip
                for v in col.values_for_doc(d) {
                    lo = lo.min(v);
                    hi = hi.max(v);
                }
            }
            // an all-missing block can never match: min > max marks it dead
            blocks.push((lo, hi));
            doc = end;
        }
        BlockStats { blocks, num_docs }
    }

    /// Fraction of blocks the range lets us skip outright. This is the whole
    /// basis for choosing between the two scans, and it costs nothing to ask.
    pub fn num_blocks(&self) -> usize {
        self.blocks.len()
    }

    pub fn skip_ratio(&self, lo: u64, hi: u64) -> f32 {
        if self.blocks.is_empty() {
            return 0.0;
        }
        let skippable = self
            .blocks
            .iter()
            .filter(|(bmin, bmax)| bmin > bmax || *bmax < lo || *bmin > hi)
            .count();
        skippable as f32 / self.blocks.len() as f32
    }

    /// Collect the documents whose value falls in `lo..=hi`.
    ///
    /// Returns the number of blocks skipped outright and taken wholesale, which
    /// is also a ready-made selectivity estimate for a planner.
    pub fn docids_in_range(
        &self,
        col: &Column<u64>,
        lo: u64,
        hi: u64,
        full: bool,
        out: &mut Vec<DocId>,
    ) -> (usize, usize) {
        let mut skipped = 0;
        let mut whole = 0;
        let mut b = 0usize;
        while b < self.blocks.len() {
            let (bmin, bmax) = self.blocks[b];
            let start = b as u32 * BLOCK;
            let end = (start + BLOCK).min(self.num_docs);
            if bmin > bmax || bmax < lo || bmin > hi {
                skipped += 1;
                b += 1;
                continue;
            }
            // taking a whole block is only sound when every document has a
            // value; otherwise the block would sweep up documents with none
            if full && bmin >= lo && bmax <= hi {
                out.extend(start..end);
                whole += 1;
                b += 1;
                continue;
            }
            // merge the run of partially covered blocks into one columnar call
            let first = b;
            while b < self.blocks.len() {
                let (m, x) = self.blocks[b];
                let dead = m > x || x < lo || m > hi;
                // the wholesale shortcut is only available on a full column, so
                // without it a block that sits inside the range is still work to
                // do -- treating it as "not partial" here left `b` unmoved and
                // spun forever
                let wholesale = full && m >= lo && x <= hi;
                if dead || wholesale {
                    break;
                }
                b += 1;
            }
            let from = first as u32 * BLOCK;
            let to = (b as u32 * BLOCK).min(self.num_docs);
            col.get_docids_for_value_range(lo..=hi, from..to, out);
        }
        (skipped, whole)
    }
}

/// Segment-scoped cache. A BoostCore segment id is stable and unique, so an entry
/// can never go stale -- a merge produces new segments with new ids.
#[derive(Default)]
pub struct StatsCache {
    inner: RwLock<HashMap<(boostcore::index::SegmentId, String), Arc<BlockStats>>>,
}

impl StatsCache {
    pub fn get_or_build(
        &self,
        reader: &SegmentReader,
        column_name: &str,
        col: &Column<u64>,
    ) -> Arc<BlockStats> {
        let key = (reader.segment_id(), column_name.to_string());
        if let Some(hit) = self.inner.read().get(&key) {
            return hit.clone();
        }
        let stats = Arc::new(BlockStats::build(col, reader.max_doc()));
        self.inner.write().insert(key, stats.clone());
        stats
    }
}

/// A range over a single-valued numeric fast field, driven by block statistics.
/// Below this share of skippable blocks the block scan is not worth it: it has
/// to materialise its whole result, while BoostCore's own range query streams and
/// lets an enclosing intersection skip ahead.
const MIN_SKIP_RATIO: f32 = 0.25;

#[derive(Clone)]
pub struct BlockRangeQuery {
    pub column: String,
    pub column_type: ColumnType,
    pub lo: u64,
    pub hi: u64,
    pub cache: Arc<StatsCache>,
    /// The general range query, used when the statistics say skipping will not
    /// pay. Carrying it here is what makes the choice a planning decision
    /// rather than a fixed strategy.
    pub fallback: Arc<dyn Query>,
}

impl std::fmt::Debug for BlockRangeQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BlockRangeQuery({}, {}..={})", self.column, self.lo, self.hi)
    }
}

impl Query for BlockRangeQuery {
    fn weight(&self, scoring: EnableScoring<'_>) -> boostcore::Result<Box<dyn Weight>> {
        // Ask the statistics whether this particular range is worth the block
        // scan, and hand the work to the general query when it is not.
        if let Some(searcher) = scoring.searcher() {
            let mut blocks = 0usize;
            let mut skipped = 0.0f32;
            for reader in searcher.segment_readers() {
                let Some(col) = self.column_for(reader) else { continue };
                let stats = self.cache.get_or_build(reader, &self.column, &col);
                let n = stats.num_blocks();
                blocks += n;
                skipped += stats.skip_ratio(self.lo, self.hi) * n as f32;
            }
            if blocks == 0 || skipped / (blocks as f32) < MIN_SKIP_RATIO {
                return self.fallback.weight(scoring);
            }
        }
        Ok(Box::new(self.clone()))
    }
}

impl BlockRangeQuery {
    fn column_for(&self, reader: &SegmentReader) -> Option<Column<u64>> {
        reader
            .fast_fields()
            .u64_lenient_for_type(Some(&[self.column_type]), &self.column)
            .ok()
            .flatten()
            .map(|(c, _)| c)
    }

    fn docids(&self, reader: &SegmentReader) -> Vec<DocId> {
        let Some(col) = self.column_for(reader) else { return Vec::new() };
        let stats = self.cache.get_or_build(reader, &self.column, &col);
        let full = col.index.get_cardinality().is_full();
        let mut out = Vec::new();
        stats.docids_in_range(&col, self.lo, self.hi, full, &mut out);
        out
    }
}

impl Weight for BlockRangeQuery {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> boostcore::Result<Box<dyn Scorer>> {
        // a bit set keeps the memory bounded by segment size rather than by how
        // many documents happen to match
        let mut bits = boostcore_common::BitSet::with_max_value(reader.max_doc());
        for doc in self.docids(reader) {
            bits.insert(doc);
        }
        Ok(Box::new(ConstScorer::new(BitSetDocSet::from(bits), boost)))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> boostcore::Result<Explanation> {
        if self.docids(reader).binary_search(&doc).is_ok() {
            Ok(Explanation::new("BlockRangeQuery", 1.0))
        } else {
            Err(TantivyError::InvalidArgument("document does not match".to_string()))
        }
    }

    fn count(&self, reader: &SegmentReader) -> boostcore::Result<u32> {
        // still has to respect deletes, so fall through to the default when any
        if reader.alive_bitset().is_some() {
            let docs = self.docids(reader);
            let alive = reader.alive_bitset().unwrap();
            return Ok(docs.into_iter().filter(|d| alive.is_alive(*d)).count() as u32);
        }
        Ok(self.docids(reader).len() as u32)
    }
}
