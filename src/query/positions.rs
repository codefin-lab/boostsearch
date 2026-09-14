//! What the positional queries share: where each word stands in a document,
//! the heap Lucene orders their matches with, and BM25 over a frequency that
//! need not be a whole number.
//!
//! A span, an interval and a sloppy phrase all score a document by walking
//! the places its words stand and adding up how close together they are.
//! The frequency that comes out is a fraction -- a match three words wide
//! counts a quarter -- and BoostCore's own BM25 takes only whole counts, so
//! the similarity is written out here the way Lucene writes it.

use super::*;
use boostcore::DocSet;
use boostcore::postings::Postings;
use boostcore::query::Bm25StatisticsProvider;

/// Where no more matches stand in the current document.
pub(crate) const NO_MORE: i32 = i32::MAX;

const K1: f32 = 1.2;
const B: f32 = 0.75;

/// Lucene's binary heap, kept to its exact order of moves.
///
/// Two entries that compare equal can come out in either order depending on
/// how the heap was filled, and a positional query that then advances the
/// one on top walks a different path than it would with the other. Keeping
/// the same moves keeps the same path.
pub(crate) struct LuceneHeap {
    heap: Vec<usize>,
}

impl LuceneHeap {
    pub(crate) fn new() -> LuceneHeap {
        LuceneHeap { heap: vec![usize::MAX] }
    }

    pub(crate) fn clear(&mut self) {
        self.heap.truncate(1);
    }

    pub(crate) fn len(&self) -> usize {
        self.heap.len() - 1
    }

    pub(crate) fn top(&self) -> Option<usize> {
        self.heap.get(1).copied()
    }

    pub(crate) fn add(&mut self, item: usize, less: &dyn Fn(usize, usize) -> bool) {
        self.heap.push(item);
        let mut i = self.len();
        let node = self.heap[i];
        let mut j = i >> 1;
        while j > 0 && less(node, self.heap[j]) {
            self.heap[i] = self.heap[j];
            i = j;
            j >>= 1;
        }
        self.heap[i] = node;
    }

    pub(crate) fn update_top(&mut self, less: &dyn Fn(usize, usize) -> bool) -> Option<usize> {
        self.down(1, less);
        self.top()
    }

    pub(crate) fn pop(&mut self, less: &dyn Fn(usize, usize) -> bool) -> Option<usize> {
        let size = self.len();
        if size == 0 {
            return None;
        }
        let result = self.heap[1];
        self.heap[1] = self.heap[size];
        self.heap.pop();
        if self.len() > 0 {
            self.down(1, less);
        }
        Some(result)
    }

    fn down(&mut self, at: usize, less: &dyn Fn(usize, usize) -> bool) {
        let size = self.len();
        if size == 0 {
            return;
        }
        let mut i = at;
        let node = self.heap[i];
        let mut j = i << 1;
        let mut k = j + 1;
        if k <= size && less(self.heap[k], self.heap[j]) {
            j = k;
        }
        while j <= size && less(self.heap[j], node) {
            self.heap[i] = self.heap[j];
            i = j;
            j = i << 1;
            k = j + 1;
            if k <= size && less(self.heap[k], self.heap[j]) {
                j = k;
            }
        }
        self.heap[i] = node;
    }
}

/// BM25 over a frequency that is a fraction, as Lucene's `BM25Similarity`
/// computes it: `weight - weight / (1 + freq * normInverse)`.
#[derive(Clone)]
pub(crate) struct Similarity {
    weight: f32,
    cache: Arc<[f32; 256]>,
}

impl Similarity {
    /// The similarity of a query over `terms`, measured against the field
    /// `probe` names. `None` where none of the words is in the index, which
    /// is where Lucene has no similarity either.
    pub(crate) fn new(
        stats: &dyn Bm25StatisticsProvider,
        probe: &Term,
        terms: &[Term],
    ) -> boostcore::Result<Option<Similarity>> {
        let (docs, tokens) = match stats.path_statistics(probe) {
            Some((docs, tokens)) if docs > 0 => (docs, tokens),
            _ => (stats.total_num_docs()?, stats.total_num_tokens(probe.field())?),
        };
        let mut idf = 0f64;
        let mut any = false;
        for term in terms {
            let df = stats.doc_freq(term)?;
            if df == 0 {
                continue;
            }
            any = true;
            let one = (1.0 + (docs as f64 - df as f64 + 0.5) / (df as f64 + 0.5)).ln() as f32;
            idf += one as f64;
        }
        if !any || docs == 0 {
            return Ok(None);
        }
        let avgdl = (tokens as f64 / docs as f64) as f32;
        let mut cache = [0f32; 256];
        for (id, slot) in cache.iter_mut().enumerate() {
            let dl = boostcore::fieldnorm::FieldNormReader::id_to_fieldnorm(id as u8) as f32;
            *slot = 1.0 / (K1 * ((1.0 - B) + B * dl / avgdl));
        }
        Ok(Some(Similarity { weight: idf as f32, cache: Arc::new(cache) }))
    }

    /// The same similarity over a field that keeps no lengths, a keyword:
    /// Lucene reads every document as one term long there, in a field whose
    /// documents hold one term each.
    pub(crate) fn without_norms(self) -> Similarity {
        Similarity { weight: self.weight, cache: Arc::new([1.0 / K1; 256]) }
    }

    pub(crate) fn score(&self, norm_id: u8, freq: f32) -> f32 {
        let inverse = self.cache[norm_id as usize];
        self.weight - self.weight / (1.0 + freq * inverse)
    }
}

/// The norms of the field a probe term names, in one segment.
pub(crate) fn norms_for(
    reader: &boostcore::SegmentReader,
    probe: &Term,
) -> boostcore::Result<Norms> {
    Ok(Norms(match reader.fieldnorms_reader_for_term(probe)? {
        Some(norms) => Some(norms),
        // a field that records no lengths -- the untouched view a keyword is
        // kept in -- has none to read, and is scored without them
        None => reader.get_fieldnorms_reader(probe.field()).ok(),
    }))
}

/// The lengths of a field in one segment, where the field keeps them.
pub(crate) struct Norms(Option<boostcore::fieldnorm::FieldNormReader>);

impl Norms {
    pub(crate) fn fieldnorm_id(&self, doc: boostcore::DocId) -> u8 {
        self.0.as_ref().map(|norms| norms.fieldnorm_id(doc)).unwrap_or(0)
    }
}

/// A term standing for the field and path another term is under, whatever
/// its value: what the norms and the collection statistics are looked up by.
pub(crate) fn probe_of(field: Field, path: &str) -> Term {
    let mut probe = Term::from_field_json_path(field, path, true);
    probe.append_type_and_str("");
    probe
}

/// The documents of one segment that hold a term, in order.
pub(crate) fn docs_of(
    reader: &boostcore::SegmentReader,
    term: &Term,
) -> boostcore::Result<Vec<boostcore::DocId>> {
    let inverted = reader.inverted_index(term.field())?;
    let mut docs = Vec::new();
    if let Some(mut postings) = inverted.read_postings(term, IndexRecordOption::Basic)? {
        while postings.doc() != boostcore::TERMINATED {
            docs.push(postings.doc());
            postings.advance();
        }
    }
    Ok(docs)
}

/// The documents both lists hold.
pub(crate) fn intersect(a: &[boostcore::DocId], b: &[boostcore::DocId]) -> Vec<boostcore::DocId> {
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// The documents either list holds.
pub(crate) fn union(a: &[boostcore::DocId], b: &[boostcore::DocId]) -> Vec<boostcore::DocId> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if j == b.len() || (i < a.len() && a[i] < b[j]) {
            out.push(a[i]);
            i += 1;
        } else if i == a.len() || b[j] < a[i] {
            out.push(b[j]);
            j += 1;
        } else {
            out.push(a[i]);
            i += 1;
            j += 1;
        }
    }
    out
}

/// The places a set of terms stands, read one document at a time and in
/// document order.
pub(crate) struct SegmentPositions {
    postings: Vec<Option<boostcore::postings::SegmentPostings>>,
    buffer: Vec<u32>,
}

impl SegmentPositions {
    pub(crate) fn open(
        reader: &boostcore::SegmentReader,
        terms: &[Term],
    ) -> boostcore::Result<SegmentPositions> {
        let mut postings = Vec::with_capacity(terms.len());
        for term in terms {
            let inverted = reader.inverted_index(term.field())?;
            postings.push(inverted.read_postings(term, IndexRecordOption::WithFreqsAndPositions)?);
        }
        Ok(SegmentPositions { postings, buffer: Vec::new() })
    }

    /// Where each term stands in `doc`; empty for a term the document does
    /// not hold. Documents have to be asked for in increasing order.
    pub(crate) fn read(&mut self, doc: boostcore::DocId, out: &mut Vec<Vec<i32>>) {
        out.resize(self.postings.len(), Vec::new());
        for (slot, postings) in out.iter_mut().zip(self.postings.iter_mut()) {
            slot.clear();
            let Some(postings) = postings else { continue };
            if postings.doc() < doc {
                postings.seek(doc);
            }
            if postings.doc() == doc {
                postings.positions(&mut self.buffer);
                slot.extend(self.buffer.iter().map(|p| *p as i32));
                // a field that keeps no positions -- a keyword's untouched
                // view -- still says how often the word is there, and a word
                // standing alone is read as standing that many times from
                // the start
                if slot.is_empty() {
                    slot.extend(0..postings.term_freq().max(1) as i32);
                }
            }
        }
    }
}
