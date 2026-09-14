//! Where a word stands in the field it was written in.
//!
//! Most queries ask whether a word is in a document. A span query asks where:
//! near another word, before the fifth one, inside another span. BoostCore
//! keeps where each word stands; the span clauses are walked over those
//! places here, the way Lucene walks them.

use super::positions::{
    LuceneHeap, NO_MORE, SegmentPositions, Similarity, docs_of, intersect, norms_for, union,
};
use super::*;
use boostcore::DocSet;
use boostcore::postings::Postings;

/// The documents a pass over the positions kept, in order.
pub(crate) struct KeptDocs {
    docs: Vec<boostcore::DocId>,
    at: usize,
}

impl KeptDocs {
    pub(crate) fn new(docs: Vec<boostcore::DocId>) -> KeptDocs {
        KeptDocs { docs, at: 0 }
    }
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
pub(crate) struct ScoredDocs {
    docs: Vec<(boostcore::DocId, boostcore::Score)>,
    at: usize,
}

impl ScoredDocs {
    pub(crate) fn new(docs: Vec<(boostcore::DocId, boostcore::Score)>) -> ScoredDocs {
        ScoredDocs { docs, at: 0 }
    }
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

/// A span query as Lucene reads it: clauses over one field, each answering
/// with the stretches of the field it matches.
///
/// The span queries used to be approximated by the queries nearest them -- a
/// phrase for `span_near`, the included clause for `span_not`, both clauses
/// for `span_containing` -- which found documents where the words stood in
/// the wrong order or too far apart, and refused the shapes a phrase cannot
/// say. What is here walks the positions the way Lucene's `Spans` classes
/// do, including where they stop short: an ordered `span_near` never goes
/// back to an earlier match of a later clause, so neither does this.
#[derive(Clone, Debug)]
pub(crate) enum SpanTree {
    Term(Term),
    Or(Vec<SpanTree>),
    Near {
        clauses: Vec<SpanTree>,
        slop: i32,
        ordered: bool,
    },
    Gap(i32),
    Not {
        include: Box<SpanTree>,
        exclude: Box<SpanTree>,
        pre: i32,
        post: i32,
    },
    First {
        inner: Box<SpanTree>,
        end: i32,
    },
    Containing {
        big: Box<SpanTree>,
        little: Box<SpanTree>,
    },
    Within {
        big: Box<SpanTree>,
        little: Box<SpanTree>,
    },
    /// a clause over one field that says it is over another, so it can
    /// stand beside that field's clauses
    Masked {
        inner: Box<SpanTree>,
        field: Term,
    },
}

impl SpanTree {
    /// The field the clause says it is over, as a term naming its path.
    pub(crate) fn field(&self) -> Option<Term> {
        match self {
            SpanTree::Term(t) => Some(t.clone()),
            SpanTree::Or(c) | SpanTree::Near { clauses: c, .. } => c.iter().find_map(|c| c.field()),
            SpanTree::Gap(_) => None,
            SpanTree::Not { include: inner, .. }
            | SpanTree::First { inner, .. }
            | SpanTree::Containing { big: inner, .. }
            | SpanTree::Within { big: inner, .. } => inner.field(),
            SpanTree::Masked { field, .. } => Some(field.clone()),
        }
    }

    /// The words whose statistics the score is made of: Lucene leaves out
    /// what `span_not` excludes, and nothing else.
    fn scored_terms(&self, out: &mut Vec<Term>) {
        match self {
            SpanTree::Term(t) => {
                if !out.contains(t) {
                    out.push(t.clone())
                }
            }
            SpanTree::Or(c) | SpanTree::Near { clauses: c, .. } => {
                c.iter().for_each(|c| c.scored_terms(out))
            }
            SpanTree::Gap(_) => {}
            SpanTree::Not { include: inner, .. }
            | SpanTree::First { inner, .. }
            | SpanTree::Masked { inner, .. } => inner.scored_terms(out),
            SpanTree::Containing { big, little } | SpanTree::Within { big, little } => {
                big.scored_terms(out);
                little.scored_terms(out);
            }
        }
    }

    /// Every word the clause reads, excluded ones too.
    fn all_terms(&self, out: &mut Vec<Term>) {
        match self {
            SpanTree::Term(t) => {
                if !out.contains(t) {
                    out.push(t.clone())
                }
            }
            SpanTree::Or(c) | SpanTree::Near { clauses: c, .. } => {
                c.iter().for_each(|c| c.all_terms(out))
            }
            SpanTree::Gap(_) => {}
            SpanTree::First { inner, .. } | SpanTree::Masked { inner, .. } => inner.all_terms(out),
            SpanTree::Not { include: big, exclude: little, .. }
            | SpanTree::Containing { big, little }
            | SpanTree::Within { big, little } => {
                big.all_terms(out);
                little.all_terms(out);
            }
        }
    }

    /// The documents of a segment that could match, `None` for all of them.
    fn candidates(
        &self,
        docs: &dyn Fn(&Term) -> Vec<boostcore::DocId>,
    ) -> Option<Vec<boostcore::DocId>> {
        fn both(
            a: Option<Vec<boostcore::DocId>>,
            b: Option<Vec<boostcore::DocId>>,
        ) -> Option<Vec<boostcore::DocId>> {
            match (a, b) {
                (None, other) | (other, None) => other,
                (Some(a), Some(b)) => Some(intersect(&a, &b)),
            }
        }
        match self {
            SpanTree::Term(t) => Some(docs(t)),
            SpanTree::Gap(_) => None,
            SpanTree::Or(c) => {
                let mut all = Some(Vec::new());
                for clause in c {
                    all = match (all, clause.candidates(docs)) {
                        (Some(a), Some(b)) => Some(union(&a, &b)),
                        _ => None,
                    };
                }
                all
            }
            SpanTree::Near { clauses, .. } => {
                clauses.iter().fold(None, |acc, clause| both(acc, clause.candidates(docs)))
            }
            SpanTree::Not { include: inner, .. }
            | SpanTree::First { inner, .. }
            | SpanTree::Masked { inner, .. } => inner.candidates(docs),
            SpanTree::Containing { big, little } | SpanTree::Within { big, little } => {
                both(big.candidates(docs), little.candidates(docs))
            }
        }
    }

    /// The spans of this clause in one document, given where each word of
    /// `terms` stands in it.
    fn spans<'a>(&self, terms: &[Term], places: &'a [Vec<i32>]) -> Box<dyn Spans + 'a> {
        match self {
            SpanTree::Term(t) => {
                let at = terms.iter().position(|x| x == t).unwrap_or(0);
                Box::new(TermSpans { places: &places[at], next: 0, current: -1 })
            }
            SpanTree::Gap(width) => Box::new(GapSpans { width: *width, at: -1 }),
            SpanTree::Or(clauses) => Box::new(OrSpans {
                subs: clauses.iter().map(|c| c.spans(terms, places)).collect(),
                alive: Vec::new(),
                heap: LuceneHeap::new(),
                top: None,
            }),
            SpanTree::Near { clauses, slop, ordered: true } => Box::new(NearOrdered {
                subs: clauses.iter().map(|c| c.spans(terms, places)).collect(),
                slop: *slop,
                start: -1,
                end: -1,
                width: -1,
                at_first: true,
                exhausted: false,
            }),
            SpanTree::Near { clauses, slop, ordered: false } => Box::new(NearUnordered {
                subs: clauses.iter().map(|c| c.spans(terms, places)).collect(),
                slop: *slop,
                heap: LuceneHeap::new(),
                total_length: 0,
                max_end: -1,
                at_first: true,
                exhausted: false,
            }),
            SpanTree::Not { include, exclude, pre, post } => Box::new(FilterSpans {
                inner: include.spans(terms, places),
                check: Check::Not {
                    exclude: exclude.spans(terms, places),
                    pre: *pre,
                    post: *post,
                    asked: false,
                    present: false,
                },
                at_first: false,
                start: -1,
            }),
            SpanTree::First { inner, end } => Box::new(FilterSpans {
                inner: inner.spans(terms, places),
                check: Check::First(*end),
                at_first: false,
                start: -1,
            }),
            SpanTree::Containing { big, little } => Box::new(ContainSpans {
                big: big.spans(terms, places),
                little: little.spans(terms, places),
                within: false,
                at_first: false,
                exhausted: false,
            }),
            SpanTree::Within { big, little } => Box::new(ContainSpans {
                big: big.spans(terms, places),
                little: little.spans(terms, places),
                within: true,
                at_first: false,
                exhausted: false,
            }),
            SpanTree::Masked { inner, .. } => inner.spans(terms, places),
        }
    }
}

/// Lucene's `Spans`, for one document: a sequence of matches, each a start
/// and an end position and a width -- how much of the match is not the words
/// themselves -- which is what the score is made of.
trait Spans {
    /// Whether the document holds a match at all. Called once, before the
    /// matches are walked, as Lucene's two-phase check is; a clause that says
    /// yes stands on its first match.
    fn matches(&mut self) -> bool;
    fn next_start(&mut self) -> i32;
    fn start(&self) -> i32;
    fn end(&self) -> i32;
    fn width(&self) -> i32;
    /// Move to the first match starting at or after `position`.
    fn skip_to(&mut self, position: i32) -> i32 {
        while self.start() < position {
            self.next_start();
        }
        self.start()
    }
}

struct TermSpans<'a> {
    places: &'a [i32],
    next: usize,
    current: i32,
}

impl Spans for TermSpans<'_> {
    fn matches(&mut self) -> bool {
        !self.places.is_empty()
    }
    fn next_start(&mut self) -> i32 {
        self.current = match self.places.get(self.next) {
            Some(p) => {
                self.next += 1;
                *p
            }
            None => NO_MORE,
        };
        self.current
    }
    fn start(&self) -> i32 {
        self.current
    }
    fn end(&self) -> i32 {
        match self.current {
            -1 => -1,
            NO_MORE => NO_MORE,
            p => p + 1,
        }
    }
    fn width(&self) -> i32 {
        0
    }
}

/// `span_gap`: every position, as a stretch of the given width.
struct GapSpans {
    width: i32,
    at: i32,
}

impl Spans for GapSpans {
    fn matches(&mut self) -> bool {
        true
    }
    fn next_start(&mut self) -> i32 {
        self.at += 1;
        self.at
    }
    fn start(&self) -> i32 {
        self.at
    }
    fn end(&self) -> i32 {
        self.at + self.width
    }
    fn width(&self) -> i32 {
        self.width
    }
    fn skip_to(&mut self, position: i32) -> i32 {
        self.at = position;
        self.at
    }
}

type SubSpans<'a> = Vec<Box<dyn Spans + 'a>>;

/// Lucene's order of matches: by start, then by end.
fn position_less(subs: &[Box<dyn Spans + '_>], a: usize, b: usize) -> bool {
    let (sa, sb) = (subs[a].start(), subs[b].start());
    sa < sb || (sa == sb && subs[a].end() < subs[b].end())
}

/// `span_or`: every match of every clause that matches, in position order.
struct OrSpans<'a> {
    subs: SubSpans<'a>,
    alive: Vec<usize>,
    heap: LuceneHeap,
    top: Option<usize>,
}

impl Spans for OrSpans<'_> {
    fn matches(&mut self) -> bool {
        self.alive = (0..self.subs.len()).filter(|i| self.subs[*i].matches()).collect();
        !self.alive.is_empty()
    }
    fn next_start(&mut self) -> i32 {
        match self.top {
            None => {
                self.heap.clear();
                for i in self.alive.clone() {
                    self.subs[i].next_start();
                    let subs = &self.subs;
                    self.heap.add(i, &|a, b| position_less(subs, a, b));
                }
                self.top = self.heap.top();
            }
            Some(top) => {
                self.subs[top].next_start();
                let subs = &self.subs;
                self.top = self.heap.update_top(&|a, b| position_less(subs, a, b));
            }
        }
        self.start()
    }
    fn start(&self) -> i32 {
        self.top.map(|t| self.subs[t].start()).unwrap_or(-1)
    }
    fn end(&self) -> i32 {
        self.top.map(|t| self.subs[t].end()).unwrap_or(-1)
    }
    fn width(&self) -> i32 {
        self.top.map(|t| self.subs[t].width()).unwrap_or(0)
    }
}

/// An ordered `span_near`: each clause after the one before it, with no more
/// than `slop` positions between them all.
struct NearOrdered<'a> {
    subs: SubSpans<'a>,
    slop: i32,
    start: i32,
    end: i32,
    width: i32,
    at_first: bool,
    exhausted: bool,
}

impl NearOrdered<'_> {
    fn stretch_to_order(&mut self) -> bool {
        self.start = self.subs[0].start();
        self.width = 0;
        for i in 1..self.subs.len() {
            let previous_end = self.subs[i - 1].end();
            if self.subs[i].skip_to(previous_end) == NO_MORE {
                self.exhausted = true;
                return false;
            }
            self.width += self.subs[i].start() - previous_end;
        }
        self.end = self.subs[self.subs.len() - 1].end();
        true
    }

    fn next_match(&mut self) -> bool {
        self.exhausted = false;
        while self.subs[0].next_start() != NO_MORE && !self.exhausted {
            if self.stretch_to_order() && self.width <= self.slop {
                return true;
            }
        }
        false
    }
}

impl Spans for NearOrdered<'_> {
    fn matches(&mut self) -> bool {
        if !self.subs.iter_mut().all(|s| s.matches()) {
            return false;
        }
        self.at_first = false;
        if self.next_match() {
            self.at_first = true;
            return true;
        }
        false
    }
    fn next_start(&mut self) -> i32 {
        if self.at_first {
            self.at_first = false;
            return self.start;
        }
        if self.next_match() {
            return self.start;
        }
        self.start = NO_MORE;
        self.end = NO_MORE;
        NO_MORE
    }
    fn start(&self) -> i32 {
        if self.at_first { -1 } else { self.start }
    }
    fn end(&self) -> i32 {
        if self.at_first { -1 } else { self.end }
    }
    fn width(&self) -> i32 {
        self.width
    }
}

/// An unordered `span_near`: one match of each clause, in any order, within
/// a window no wider than the clauses themselves plus `slop`.
struct NearUnordered<'a> {
    subs: SubSpans<'a>,
    slop: i32,
    heap: LuceneHeap,
    total_length: i32,
    max_end: i32,
    at_first: bool,
    exhausted: bool,
}

impl NearUnordered<'_> {
    fn top(&self) -> usize {
        self.heap.top().unwrap_or(0)
    }

    fn start_document(&mut self) {
        self.heap.clear();
        self.total_length = 0;
        self.max_end = -1;
        for i in 0..self.subs.len() {
            self.subs[i].next_start();
            let subs = &self.subs;
            self.heap.add(i, &|a, b| position_less(subs, a, b));
            let (start, end) = (self.subs[i].start(), self.subs[i].end());
            self.max_end = self.max_end.max(end);
            self.total_length += end - start;
        }
    }

    fn next_position(&mut self) -> bool {
        let top = self.top();
        self.total_length -= self.subs[top].end() - self.subs[top].start();
        if self.subs[top].next_start() == NO_MORE {
            return false;
        }
        self.total_length += self.subs[top].end() - self.subs[top].start();
        self.max_end = self.max_end.max(self.subs[top].end());
        let subs = &self.subs;
        self.heap.update_top(&|a, b| position_less(subs, a, b));
        true
    }

    fn at_match(&self) -> bool {
        let start = self.subs[self.top()].start() as i64;
        self.max_end as i64 - start - self.total_length as i64 <= self.slop as i64
    }
}

impl Spans for NearUnordered<'_> {
    fn matches(&mut self) -> bool {
        if !self.subs.iter_mut().all(|s| s.matches()) {
            return false;
        }
        self.at_first = false;
        self.exhausted = false;
        self.start_document();
        loop {
            if self.at_match() {
                self.at_first = true;
                return true;
            }
            if !self.next_position() {
                return false;
            }
        }
    }
    fn next_start(&mut self) -> i32 {
        if self.at_first {
            self.at_first = false;
            return self.subs[self.top()].start();
        }
        loop {
            if !self.next_position() {
                self.exhausted = true;
                return NO_MORE;
            }
            if self.at_match() {
                return self.subs[self.top()].start();
            }
        }
    }
    fn start(&self) -> i32 {
        match (self.at_first, self.exhausted) {
            (true, _) => -1,
            (_, true) => NO_MORE,
            _ => self.subs[self.top()].start(),
        }
    }
    fn end(&self) -> i32 {
        match (self.at_first, self.exhausted) {
            (true, _) => -1,
            (_, true) => NO_MORE,
            _ => self.max_end,
        }
    }
    fn width(&self) -> i32 {
        self.max_end - self.subs[self.top()].start()
    }
}

/// What a filtering clause asks of each match it passes on.
enum Check<'a> {
    /// `span_first`: the match ends by this position
    First(i32),
    /// `span_not`: no match of `exclude` within `pre` before or `post` after
    Not { exclude: Box<dyn Spans + 'a>, pre: i32, post: i32, asked: bool, present: bool },
}

enum Accept {
    Yes,
    No,
    NoMoreInDocument,
}

/// `span_first` and `span_not`: the matches of one clause that pass a check.
struct FilterSpans<'a> {
    inner: Box<dyn Spans + 'a>,
    check: Check<'a>,
    at_first: bool,
    start: i32,
}

impl FilterSpans<'_> {
    fn accept(&mut self) -> Accept {
        let (start, end) = (self.inner.start(), self.inner.end());
        match &mut self.check {
            Check::First(limit) => {
                if start >= *limit {
                    Accept::NoMoreInDocument
                } else if end <= *limit {
                    Accept::Yes
                } else {
                    Accept::No
                }
            }
            Check::Not { exclude, pre, post, asked, present } => {
                if !*asked {
                    *asked = true;
                    *present = exclude.matches();
                }
                if !*present {
                    return Accept::Yes;
                }
                if exclude.start() == -1 {
                    exclude.next_start();
                }
                while exclude.end() as i64 <= start as i64 - *pre as i64 {
                    if exclude.next_start() == NO_MORE {
                        return Accept::Yes;
                    }
                }
                if exclude.start() as i64 - *post as i64 >= end as i64 {
                    Accept::Yes
                } else {
                    Accept::No
                }
            }
        }
    }
}

impl Spans for FilterSpans<'_> {
    fn matches(&mut self) -> bool {
        if !self.inner.matches() {
            return false;
        }
        self.at_first = false;
        self.start = self.inner.next_start();
        loop {
            match self.accept() {
                Accept::Yes => {
                    self.at_first = true;
                    return true;
                }
                Accept::No => {
                    self.start = self.inner.next_start();
                    if self.start == NO_MORE {
                        self.start = -1;
                        return false;
                    }
                }
                Accept::NoMoreInDocument => {
                    self.start = -1;
                    return false;
                }
            }
        }
    }
    fn next_start(&mut self) -> i32 {
        if self.at_first {
            self.at_first = false;
            return self.start;
        }
        loop {
            self.start = self.inner.next_start();
            if self.start == NO_MORE {
                return NO_MORE;
            }
            match self.accept() {
                Accept::Yes => return self.start,
                Accept::No => {}
                Accept::NoMoreInDocument => {
                    self.start = NO_MORE;
                    return NO_MORE;
                }
            }
        }
    }
    fn start(&self) -> i32 {
        if self.at_first { -1 } else { self.start }
    }
    fn end(&self) -> i32 {
        if self.at_first {
            -1
        } else if self.start != NO_MORE {
            self.inner.end()
        } else {
            NO_MORE
        }
    }
    fn width(&self) -> i32 {
        self.inner.width()
    }
}

/// `span_containing` answers with the big matches that hold a little one,
/// `span_within` with the little matches a big one holds.
struct ContainSpans<'a> {
    big: Box<dyn Spans + 'a>,
    little: Box<dyn Spans + 'a>,
    within: bool,
    at_first: bool,
    exhausted: bool,
}

impl ContainSpans<'_> {
    /// The next match of the answering clause that the other one allows.
    fn advance(&mut self) -> bool {
        if self.within {
            while self.little.next_start() != NO_MORE {
                while self.big.end() < self.little.end() {
                    if self.big.next_start() == NO_MORE {
                        self.exhausted = true;
                        return false;
                    }
                }
                if self.big.start() <= self.little.start() {
                    return true;
                }
            }
        } else {
            while self.big.next_start() != NO_MORE {
                while self.little.start() < self.big.start() {
                    if self.little.next_start() == NO_MORE {
                        self.exhausted = true;
                        return false;
                    }
                }
                if self.big.end() >= self.little.end() {
                    return true;
                }
            }
        }
        self.exhausted = true;
        false
    }

    fn source(&self) -> &dyn Spans {
        if self.within { self.little.as_ref() } else { self.big.as_ref() }
    }
}

impl Spans for ContainSpans<'_> {
    fn matches(&mut self) -> bool {
        if !self.big.matches() || !self.little.matches() {
            return false;
        }
        self.at_first = false;
        self.exhausted = false;
        if self.advance() {
            self.at_first = true;
            return true;
        }
        false
    }
    fn next_start(&mut self) -> i32 {
        if self.at_first {
            self.at_first = false;
            return self.source().start();
        }
        match self.advance() {
            true => self.source().start(),
            false => NO_MORE,
        }
    }
    fn start(&self) -> i32 {
        match (self.at_first, self.exhausted) {
            (true, _) => -1,
            (_, true) => NO_MORE,
            _ => self.source().start(),
        }
    }
    fn end(&self) -> i32 {
        match (self.at_first, self.exhausted) {
            (true, _) => -1,
            (_, true) => NO_MORE,
            _ => self.source().end(),
        }
    }
    fn width(&self) -> i32 {
        self.source().width()
    }
}

/// A span query, scored as Lucene's `SpanWeight` scores one: BM25 over the
/// idf of every word it reads, with each match counting `1 / (1 + width)`.
#[derive(Clone, Debug)]
pub(crate) struct SpanQuery {
    tree: SpanTree,
    /// whether the field keeps how long each document's value is
    norms: bool,
}

impl SpanQuery {
    pub(crate) fn new(tree: SpanTree) -> SpanQuery {
        SpanQuery { tree, norms: true }
    }

    /// The query over a field that keeps no lengths, such as a keyword.
    pub(crate) fn without_norms(mut self) -> SpanQuery {
        self.norms = false;
        self
    }
}

impl Query for SpanQuery {
    fn weight(&self, scoring: EnableScoring<'_>) -> boostcore::Result<Box<dyn Weight>> {
        // a masking clause on its own is its inner clause: the field it
        // claims only matters beside the clauses of that field
        let mut root = &self.tree;
        while let SpanTree::Masked { inner, .. } = root {
            root = inner;
        }
        let mut scored = Vec::new();
        root.scored_terms(&mut scored);
        let mut terms = Vec::new();
        root.all_terms(&mut terms);
        let probe = root.field();
        let similarity = match (scoring, &probe) {
            (EnableScoring::Enabled { statistics_provider, .. }, Some(probe)) => {
                Similarity::new(statistics_provider, probe, &scored)?
            }
            _ => None,
        };
        let similarity = match self.norms {
            true => similarity,
            false => similarity.map(Similarity::without_norms),
        };
        Ok(Box::new(SpanWeight { tree: root.clone(), terms, probe, similarity }))
    }
}

struct SpanWeight {
    tree: SpanTree,
    terms: Vec<Term>,
    probe: Option<Term>,
    similarity: Option<Similarity>,
}

impl SpanWeight {
    /// Each document of the segment that matches, with its sloppy frequency.
    fn frequencies(
        &self,
        reader: &boostcore::SegmentReader,
        only: Option<boostcore::DocId>,
    ) -> boostcore::Result<Vec<(boostcore::DocId, f32)>> {
        let mut lists: Vec<Vec<boostcore::DocId>> = Vec::new();
        for term in &self.terms {
            lists.push(docs_of(reader, term)?);
        }
        let lookup = |t: &Term| {
            self.terms.iter().position(|x| x == t).map(|i| lists[i].clone()).unwrap_or_default()
        };
        let mut candidates = match self.tree.candidates(&lookup) {
            Some(docs) => docs,
            None => (0..reader.max_doc()).collect(),
        };
        if let Some(doc) = only {
            candidates.retain(|d| *d == doc);
        }
        let mut positions = SegmentPositions::open(reader, &self.terms)?;
        let mut places = Vec::new();
        let mut out = Vec::new();
        for doc in candidates {
            positions.read(doc, &mut places);
            let mut spans = self.tree.spans(&self.terms, &places);
            if !spans.matches() {
                continue;
            }
            let mut freq = 0f32;
            while spans.next_start() != NO_MORE {
                freq = (freq as f64 + 1.0 / (1.0 + spans.width() as f64)) as f32;
            }
            out.push((doc, freq));
        }
        Ok(out)
    }
}

impl Weight for SpanWeight {
    fn scorer(
        &self,
        reader: &boostcore::SegmentReader,
        boost: boostcore::Score,
    ) -> boostcore::Result<Box<dyn boostcore::query::Scorer>> {
        let found = self.frequencies(reader, None)?;
        let (Some(similarity), Some(probe)) = (&self.similarity, &self.probe) else {
            let docs = found.into_iter().map(|(doc, _)| doc).collect();
            return Ok(Box::new(boostcore::query::ConstScorer::new(KeptDocs::new(docs), boost)));
        };
        let norms = norms_for(reader, probe)?;
        let scored = found
            .into_iter()
            .map(|(doc, freq)| (doc, boost * similarity.score(norms.fieldnorm_id(doc), freq)))
            .collect();
        Ok(Box::new(ScoredDocs::new(scored)))
    }

    fn explain(
        &self,
        reader: &boostcore::SegmentReader,
        doc: boostcore::DocId,
    ) -> boostcore::Result<boostcore::query::Explanation> {
        let Some((_, freq)) = self.frequencies(reader, Some(doc))?.into_iter().next() else {
            return Err(boostcore::TantivyError::InvalidArgument(
                "document does not match the span query".to_string(),
            ));
        };
        let score = match (&self.similarity, &self.probe) {
            (Some(similarity), Some(probe)) => {
                similarity.score(norms_for(reader, probe)?.fieldnorm_id(doc), freq)
            }
            _ => 1.0,
        };
        let mut explanation =
            boostcore::query::Explanation::new("weight(spans), result of: score(freq)", score);
        explanation.add_const("freq, sloppy frequency of the spans", freq);
        Ok(explanation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(word: &str) -> Term {
        Term::from_field_text(Field::from_field_id(0), word)
    }

    fn word(w: &str) -> SpanTree {
        SpanTree::Term(term(w))
    }

    fn near(clauses: Vec<SpanTree>, slop: i32, ordered: bool) -> SpanTree {
        SpanTree::Near { clauses, slop, ordered }
    }

    /// Every match of a clause in one document, as start, end and width.
    fn matches(tree: &SpanTree, doc: &[(&str, &[i32])]) -> Vec<(i32, i32, i32)> {
        let terms: Vec<Term> = doc.iter().map(|(w, _)| term(w)).collect();
        let places: Vec<Vec<i32>> = doc.iter().map(|(_, p)| p.to_vec()).collect();
        let mut spans = tree.spans(&terms, &places);
        let mut out = Vec::new();
        if spans.matches() {
            while spans.next_start() != NO_MORE {
                out.push((spans.start(), spans.end(), spans.width()));
            }
        }
        out
    }

    // "either party may terminate this agreement immediately upon written notice"
    const CAUSE: &[(&str, &[i32])] = &[("terminate", &[3]), ("notice", &[9]), ("party", &[1])];

    #[test]
    fn an_ordered_near_keeps_its_order() {
        assert!(matches(&near(vec![word("notice"), word("terminate")], 7, true), CAUSE).is_empty());
        assert_eq!(
            matches(&near(vec![word("terminate"), word("notice")], 5, true), CAUSE),
            vec![(3, 10, 5)]
        );
        assert!(matches(&near(vec![word("terminate"), word("notice")], 4, true), CAUSE).is_empty());
    }

    #[test]
    fn an_unordered_near_takes_either_order() {
        assert_eq!(
            matches(&near(vec![word("notice"), word("terminate")], 5, false), CAUSE),
            vec![(3, 10, 7)]
        );
    }

    #[test]
    fn span_not_drops_a_match_the_exclusion_overlaps() {
        // "neither party shall be liable"
        let doc: &[(&str, &[i32])] = &[("liable", &[4]), ("neither", &[0])];
        let exclude = near(vec![word("neither"), word("liable")], 3, true);
        let not = SpanTree::Not {
            include: Box::new(word("liable")),
            exclude: Box::new(exclude),
            pre: 0,
            post: 0,
        };
        assert!(matches(&not, doc).is_empty());
    }

    #[test]
    fn span_first_reads_the_end_of_a_near() {
        let first = |end| SpanTree::First {
            inner: Box::new(near(vec![word("party"), word("terminate")], 2, true)),
            end,
        };
        assert!(matches(&first(3), CAUSE).is_empty());
        assert_eq!(matches(&first(4), CAUSE), vec![(1, 4, 1)]);
    }
}
