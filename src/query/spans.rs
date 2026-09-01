//! Where a word stands in the field it was written in.
//!
//! Most queries ask whether a word is in a document. A span query asks where:
//! near another word, before the fifth one, inside another span. What is here
//! is the part of that BoostCore can answer for on its own -- a word, and how
//! early in the field it stands.

use super::*;
use boostcore::DocSet;
use boostcore::postings::Postings;

/// A term that stands within the first `end` positions of its field.
pub(crate) struct FirstPositions {
    term: Term,
    end: usize,
}

impl FirstPositions {
    pub(crate) fn new(term: Term, end: usize) -> FirstPositions {
        FirstPositions { term, end }
    }
}

impl std::fmt::Debug for FirstPositions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FirstPositions(end={})", self.end)
    }
}

impl Clone for FirstPositions {
    fn clone(&self) -> Self {
        FirstPositions { term: self.term.clone(), end: self.end }
    }
}

impl Query for FirstPositions {
    fn weight(&self, _scoring: EnableScoring<'_>) -> boostcore::Result<Box<dyn Weight>> {
        Ok(Box::new(FirstPositionsWeight { term: self.term.clone(), end: self.end }))
    }
}

struct FirstPositionsWeight {
    term: Term,
    end: usize,
}

impl Weight for FirstPositionsWeight {
    fn scorer(
        &self,
        reader: &boostcore::SegmentReader,
        boost: boostcore::Score,
    ) -> boostcore::Result<Box<dyn boostcore::query::Scorer>> {
        let inverted = reader.inverted_index(self.term.field())?;
        let mut kept: Vec<boostcore::DocId> = Vec::new();
        if let Some(mut postings) =
            inverted.read_postings(&self.term, IndexRecordOption::WithFreqsAndPositions)?
        {
            let mut positions = Vec::new();
            while postings.doc() != boostcore::TERMINATED {
                postings.positions(&mut positions);
                // the word has to stand within the first `end` places
                if positions.iter().any(|at| (*at as usize) < self.end) {
                    kept.push(postings.doc());
                }
                postings.advance();
            }
        }
        Ok(Box::new(boostcore::query::ConstScorer::new(KeptDocs { docs: kept, at: 0 }, boost)))
    }

    fn explain(
        &self,
        _reader: &boostcore::SegmentReader,
        _doc: boostcore::DocId,
    ) -> boostcore::Result<boostcore::query::Explanation> {
        Ok(boostcore::query::Explanation::new("span_first", 1.0))
    }
}

/// The documents a pass over the positions kept, in order.
struct KeptDocs {
    docs: Vec<boostcore::DocId>,
    at: usize,
}

impl boostcore::DocSet for KeptDocs {
    fn advance(&mut self) -> boostcore::DocId {
        self.at += 1;
        self.doc()
    }

    fn doc(&self) -> boostcore::DocId {
        match self.docs.get(self.at) {
            Some(doc) => *doc,
            None => boostcore::TERMINATED,
        }
    }

    fn size_hint(&self) -> u32 {
        self.docs.len() as u32
    }
}
