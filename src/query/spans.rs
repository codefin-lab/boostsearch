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

/// Any of a set of words, scored as one span query rather than as a bool.
///
/// Lucene reads a span query as a single thing standing in the field: the
/// weight it builds carries the idf of every word in the query together, and
/// the frequency it scores is how many times any of them stands there. A bool
/// over the same words scores each separately and adds the scores, which
/// ranks a rare word far above a common one in a short field where Lucene
/// ranks the short field first.
pub(crate) struct SpanUnion {
    terms: Vec<Term>,
    /// whether the words are scored as merely there: once, in a field of no
    /// particular length, which is how a field that keeps neither
    /// frequencies nor norms scores
    flat: bool,
}

impl SpanUnion {
    pub(crate) fn new(terms: Vec<Term>) -> SpanUnion {
        SpanUnion { terms, flat: false }
    }

    /// A word that is either there or not, scored the same wherever it is.
    pub(crate) fn flat(term: Term) -> SpanUnion {
        SpanUnion { terms: vec![term], flat: true }
    }
}

impl std::fmt::Debug for SpanUnion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SpanUnion({} terms)", self.terms.len())
    }
}

impl Clone for SpanUnion {
    fn clone(&self) -> Self {
        SpanUnion { terms: self.terms.clone(), flat: self.flat }
    }
}

impl Query for SpanUnion {
    fn weight(&self, scoring: EnableScoring<'_>) -> boostcore::Result<Box<dyn Weight>> {
        let bm25 = match scoring {
            EnableScoring::Enabled { statistics_provider, .. } => {
                Some(boostcore::query::Bm25Weight::for_terms(statistics_provider, &self.terms)?)
            }
            EnableScoring::Disabled { .. } => None,
        };
        Ok(Box::new(SpanUnionWeight { terms: self.terms.clone(), bm25, flat: self.flat }))
    }
}

struct SpanUnionWeight {
    terms: Vec<Term>,
    bm25: Option<boostcore::query::Bm25Weight>,
    flat: bool,
}

impl SpanUnionWeight {
    /// How often any of the words stands in each document of this segment.
    fn frequencies(
        &self,
        reader: &boostcore::SegmentReader,
    ) -> boostcore::Result<Vec<(boostcore::DocId, u32)>> {
        let mut totals: std::collections::BTreeMap<boostcore::DocId, u32> =
            std::collections::BTreeMap::new();
        for term in &self.terms {
            let inverted = reader.inverted_index(term.field())?;
            let Some(mut postings) = inverted.read_postings(term, IndexRecordOption::WithFreqs)?
            else {
                continue;
            };
            while postings.doc() != boostcore::TERMINATED {
                *totals.entry(postings.doc()).or_default() += postings.term_freq();
                postings.advance();
            }
        }
        Ok(totals.into_iter().collect())
    }
}

impl Weight for SpanUnionWeight {
    fn scorer(
        &self,
        reader: &boostcore::SegmentReader,
        boost: boostcore::Score,
    ) -> boostcore::Result<Box<dyn boostcore::query::Scorer>> {
        let found = self.frequencies(reader)?;
        let Some(bm25) = self.bm25.clone() else {
            let docs = found.into_iter().map(|(doc, _)| doc).collect();
            return Ok(Box::new(boostcore::query::ConstScorer::new(
                KeptDocs { docs, at: 0 },
                boost,
            )));
        };
        let Some(first) = self.terms.first() else {
            return Ok(Box::new(boostcore::query::EmptyScorer));
        };
        let norms = match reader.fieldnorms_reader_for_term(first)? {
            Some(norms) => norms,
            None => reader.get_fieldnorms_reader(first.field())?,
        };
        // a flat score reads every document as one word long holding the
        // term once
        let one = boostcore::fieldnorm::FieldNormReader::fieldnorm_to_id(1);
        let flat = self.flat;
        let scored = found
            .into_iter()
            .map(|(doc, freq)| match flat {
                true => (doc, boost * bm25.score(one, 1)),
                false => (doc, boost * bm25.score(norms.fieldnorm_id(doc), freq)),
            })
            .collect();
        Ok(Box::new(ScoredDocs { docs: scored, at: 0 }))
    }

    fn explain(
        &self,
        reader: &boostcore::SegmentReader,
        doc: boostcore::DocId,
    ) -> boostcore::Result<boostcore::query::Explanation> {
        let freq = self.frequencies(reader)?.into_iter().find(|(at, _)| *at == doc).map(|(_, f)| f);
        let Some(freq) = freq else {
            return Err(boostcore::TantivyError::InvalidArgument(
                "document does not match the span query".to_string(),
            ));
        };
        match (&self.bm25, self.terms.first()) {
            (Some(bm25), Some(term)) => {
                let norms = match reader.fieldnorms_reader_for_term(term)? {
                    Some(norms) => norms,
                    None => reader.get_fieldnorms_reader(term.field())?,
                };
                Ok(bm25.explain(norms.fieldnorm_id(doc), freq))
            }
            _ => Ok(boostcore::query::Explanation::new("span", 1.0)),
        }
    }
}

/// The documents a span union matched, each with the score it was given.
struct ScoredDocs {
    docs: Vec<(boostcore::DocId, boostcore::Score)>,
    at: usize,
}

impl boostcore::DocSet for ScoredDocs {
    fn advance(&mut self) -> boostcore::DocId {
        self.at += 1;
        self.doc()
    }

    fn doc(&self) -> boostcore::DocId {
        match self.docs.get(self.at) {
            Some((doc, _)) => *doc,
            None => boostcore::TERMINATED,
        }
    }

    fn size_hint(&self) -> u32 {
        self.docs.len() as u32
    }
}

impl boostcore::query::Scorer for ScoredDocs {
    fn score(&mut self) -> boostcore::Score {
        self.docs.get(self.at).map(|(_, score)| *score).unwrap_or(0.0)
    }
}

/// Several ways of reading a phrase, scored as one span query.
///
/// Lucene reads a phrase over a token graph as one span query -- an `or` of
/// the ways through it -- and weighs it once: the idf of every word in every
/// way added together, and the frequency being how often any way stands in
/// the document. Scored as a bool of phrases instead, a way through rare
/// words would outrank a short document that holds a common one, which is
/// the opposite of what OpenSearch answers.
pub(crate) struct SpanPaths {
    terms: Vec<Term>,
    ways: Vec<Box<dyn Query>>,
}

impl SpanPaths {
    pub(crate) fn new(terms: Vec<Term>, ways: Vec<Box<dyn Query>>) -> SpanPaths {
        let mut distinct: Vec<Term> = Vec::new();
        for term in terms {
            if !distinct.contains(&term) {
                distinct.push(term);
            }
        }
        SpanPaths { terms: distinct, ways }
    }
}

impl std::fmt::Debug for SpanPaths {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SpanPaths({} ways over {} terms)", self.ways.len(), self.terms.len())
    }
}

impl Clone for SpanPaths {
    fn clone(&self) -> Self {
        SpanPaths {
            terms: self.terms.clone(),
            ways: self.ways.iter().map(|w| w.box_clone()).collect(),
        }
    }
}

impl Query for SpanPaths {
    fn weight(&self, scoring: EnableScoring<'_>) -> boostcore::Result<Box<dyn Weight>> {
        let bm25 = match scoring {
            EnableScoring::Enabled { statistics_provider, .. } if !self.terms.is_empty() => {
                Some(boostcore::query::Bm25Weight::for_terms(statistics_provider, &self.terms)?)
            }
            _ => None,
        };
        let ways: boostcore::Result<Vec<Box<dyn Weight>>> =
            self.ways.iter().map(|way| way.weight(scoring)).collect();
        Ok(Box::new(SpanPathsWeight { first: self.terms.first().cloned(), ways: ways?, bm25 }))
    }
}

struct SpanPathsWeight {
    first: Option<Term>,
    ways: Vec<Box<dyn Weight>>,
    bm25: Option<boostcore::query::Bm25Weight>,
}

impl SpanPathsWeight {
    /// How many of the ways stand in each document of this segment.
    fn frequencies(
        &self,
        reader: &boostcore::SegmentReader,
    ) -> boostcore::Result<Vec<(boostcore::DocId, u32)>> {
        let mut totals: std::collections::BTreeMap<boostcore::DocId, u32> =
            std::collections::BTreeMap::new();
        for way in &self.ways {
            let mut scorer = way.scorer(reader, 1.0)?;
            while scorer.doc() != boostcore::TERMINATED {
                *totals.entry(scorer.doc()).or_default() += 1;
                scorer.advance();
            }
        }
        Ok(totals.into_iter().collect())
    }
}

impl Weight for SpanPathsWeight {
    fn scorer(
        &self,
        reader: &boostcore::SegmentReader,
        boost: boostcore::Score,
    ) -> boostcore::Result<Box<dyn boostcore::query::Scorer>> {
        let found = self.frequencies(reader)?;
        let (Some(bm25), Some(first)) = (self.bm25.clone(), self.first.as_ref()) else {
            let docs = found.into_iter().map(|(doc, _)| doc).collect();
            return Ok(Box::new(boostcore::query::ConstScorer::new(
                KeptDocs { docs, at: 0 },
                boost,
            )));
        };
        let norms = match reader.fieldnorms_reader_for_term(first)? {
            Some(norms) => norms,
            None => reader.get_fieldnorms_reader(first.field())?,
        };
        let scored = found
            .into_iter()
            .map(|(doc, freq)| (doc, boost * bm25.score(norms.fieldnorm_id(doc), freq)))
            .collect();
        Ok(Box::new(ScoredDocs { docs: scored, at: 0 }))
    }

    fn explain(
        &self,
        reader: &boostcore::SegmentReader,
        doc: boostcore::DocId,
    ) -> boostcore::Result<boostcore::query::Explanation> {
        let freq = self.frequencies(reader)?.into_iter().find(|(at, _)| *at == doc).map(|(_, f)| f);
        let Some(freq) = freq else {
            return Err(boostcore::TantivyError::InvalidArgument(
                "document does not match the span query".to_string(),
            ));
        };
        match (&self.bm25, self.first.as_ref()) {
            (Some(bm25), Some(term)) => {
                let norms = match reader.fieldnorms_reader_for_term(term)? {
                    Some(norms) => norms,
                    None => reader.get_fieldnorms_reader(term.field())?,
                };
                Ok(bm25.explain(norms.fieldnorm_id(doc), freq))
            }
            _ => Ok(boostcore::query::Explanation::new("span", 1.0)),
        }
    }
}
