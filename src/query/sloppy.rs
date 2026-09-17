//! A phrase with room between its words, matched and scored as Lucene's
//! `SloppyPhraseMatcher` does it.
//!
//! VeloCore's phrase with slop counts every document where the words fit as
//! one occurrence, however many times they do and however loosely. Lucene
//! counts each match, and weighs it by how far the words had to move to make
//! it: a match `n` moves long adds `1 / (1 + n)`. So `notify in writing` at
//! slop 2 against `notify the supplier in writing` scores a third of an exact
//! phrase here, where it had scored as much as one. The walk below is
//! Lucene's, word for word, including how it keeps a repeated word from
//! standing in for itself.

use super::positions::{LuceneHeap, SegmentPositions, Similarity, docs_of, intersect, norms_for};
use super::*;

#[derive(Clone, Debug)]
pub(crate) struct SloppyPhrase {
    terms: Vec<Term>,
    /// where each word stands in the phrase, gaps left by the analyzer kept
    offsets: Vec<usize>,
    slop: i32,
}

impl SloppyPhrase {
    pub(crate) fn new(terms: Vec<Term>, offsets: Vec<usize>, slop: u32) -> SloppyPhrase {
        SloppyPhrase { terms, offsets, slop: slop.min(i32::MAX as u32) as i32 }
    }
}

impl Query for SloppyPhrase {
    fn weight(&self, scoring: EnableScoring<'_>) -> velocore::Result<Box<dyn Weight>> {
        // every word counts toward the idf, a repeated one as often as it is
        // written, which is how Lucene's phrase weight gathers them
        let similarity = match (scoring, self.terms.first()) {
            (EnableScoring::Enabled { statistics_provider, .. }, Some(first)) => {
                Similarity::new(statistics_provider, first, &self.terms)?
            }
            _ => None,
        };
        let mut distinct: Vec<Term> = Vec::new();
        for term in &self.terms {
            if !distinct.contains(term) {
                distinct.push(term.clone());
            }
        }
        let ords = self.terms.iter().map(|t| distinct.iter().position(|d| d == t).unwrap_or(0));
        Ok(Box::new(SloppyWeight {
            ords: ords.collect(),
            offsets: self.offsets.clone(),
            distinct,
            slop: self.slop,
            similarity,
        }))
    }
}

struct SloppyWeight {
    /// which distinct word each word of the phrase is
    ords: Vec<usize>,
    offsets: Vec<usize>,
    distinct: Vec<Term>,
    slop: i32,
    similarity: Option<Similarity>,
}

impl SloppyWeight {
    fn frequencies(
        &self,
        reader: &velocore::SegmentReader,
        only: Option<velocore::DocId>,
    ) -> velocore::Result<Vec<(velocore::DocId, f32)>> {
        let mut candidates: Option<Vec<velocore::DocId>> = None;
        for term in &self.distinct {
            let docs = docs_of(reader, term)?;
            candidates = Some(match candidates {
                None => docs,
                Some(before) => intersect(&before, &docs),
            });
        }
        let mut candidates = candidates.unwrap_or_default();
        if let Some(doc) = only {
            candidates.retain(|d| *d == doc);
        }
        let mut positions = SegmentPositions::open(reader, &self.distinct)?;
        let mut places = Vec::new();
        let mut matcher = Matcher::new(&self.ords, &self.offsets, self.slop);
        let mut out = Vec::new();
        for doc in candidates {
            positions.read(doc, &mut places);
            if !matcher.reset(&places) || !matcher.next_match(&places) {
                continue;
            }
            let mut freq = matcher.sloppy_weight();
            while matcher.next_match(&places) {
                freq += matcher.sloppy_weight();
            }
            out.push((doc, freq));
        }
        Ok(out)
    }
}

impl Weight for SloppyWeight {
    fn scorer(
        &self,
        reader: &velocore::SegmentReader,
        boost: velocore::Score,
    ) -> velocore::Result<Box<dyn velocore::query::Scorer>> {
        let found = self.frequencies(reader, None)?;
        let (Some(similarity), Some(first)) = (&self.similarity, self.distinct.first()) else {
            let docs = found.into_iter().map(|(doc, _)| doc).collect();
            return Ok(Box::new(velocore::query::ConstScorer::new(KeptDocs::new(docs), boost)));
        };
        let norms = norms_for(reader, first)?;
        let scored = found
            .into_iter()
            .map(|(doc, freq)| (doc, boost * similarity.score(norms.fieldnorm_id(doc), freq)))
            .collect();
        Ok(Box::new(ScoredDocs::new(scored)))
    }

    fn explain(
        &self,
        reader: &velocore::SegmentReader,
        doc: velocore::DocId,
    ) -> velocore::Result<velocore::query::Explanation> {
        let Some((_, freq)) = self.frequencies(reader, Some(doc))?.into_iter().next() else {
            return Err(velocore::TantivyError::InvalidArgument(
                "document does not match the phrase".to_string(),
            ));
        };
        let score = match (&self.similarity, self.distinct.first()) {
            (Some(similarity), Some(first)) => {
                similarity.score(norms_for(reader, first)?.fieldnorm_id(doc), freq)
            }
            _ => 1.0,
        };
        let mut explanation =
            velocore::query::Explanation::new("weight(phrase), result of: score(freq)", score);
        explanation.add_const("phraseFreq, sloppy frequency of the phrase", freq);
        Ok(explanation)
    }
}

/// One word of the phrase, walking its places in the document.
#[derive(Clone)]
struct PhrasePositions {
    /// where it stands, less its place in the phrase
    position: i32,
    count: i64,
    next: usize,
    offset: i32,
    word: usize,
    group: i32,
    index: usize,
}

struct Matcher {
    pps: Vec<PhrasePositions>,
    /// the words written more than once, each group by place in the phrase
    groups: Vec<Vec<usize>>,
    slop: i32,
    end: i32,
    heap: LuceneHeap,
    positioned: bool,
    match_length: i32,
}

impl Matcher {
    fn new(words: &[usize], offsets: &[usize], slop: i32) -> Matcher {
        let mut pps: Vec<PhrasePositions> = words
            .iter()
            .enumerate()
            .map(|(i, word)| PhrasePositions {
                position: 0,
                count: 0,
                next: 0,
                offset: offsets.get(i).copied().unwrap_or(i) as i32,
                word: *word,
                group: -1,
                index: 0,
            })
            .collect();
        // a word that stands twice in the phrase: the two places are a group,
        // in the order they are written
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for i in 0..pps.len() {
            if pps[i].group >= 0 {
                continue;
            }
            for j in i + 1..pps.len() {
                if pps[j].group >= 0 || pps[j].word != pps[i].word {
                    continue;
                }
                let g = match pps[i].group {
                    g if g >= 0 => g as usize,
                    _ => {
                        pps[i].group = groups.len() as i32;
                        groups.push(vec![i]);
                        groups.len() - 1
                    }
                };
                pps[j].group = g as i32;
                groups[g].push(j);
            }
        }
        for group in &groups {
            for (index, pp) in group.iter().enumerate() {
                pps[*pp].index = index;
            }
        }
        Matcher {
            pps,
            groups,
            slop,
            end: i32::MIN,
            heap: LuceneHeap::new(),
            positioned: false,
            match_length: i32::MAX,
        }
    }

    fn less(pps: &[PhrasePositions], a: usize, b: usize) -> bool {
        let (x, y) = (&pps[a], &pps[b]);
        if x.position == y.position {
            if x.offset == y.offset { a < b } else { x.offset < y.offset }
        } else {
            x.position < y.position
        }
    }

    fn next_position(&mut self, pp: usize, places: &[Vec<i32>]) -> bool {
        let p = &mut self.pps[pp];
        let had = p.count;
        p.count -= 1;
        if had > 0 {
            p.position = places[p.word][p.next] - p.offset;
            p.next += 1;
            true
        } else {
            false
        }
    }

    fn first_position(&mut self, pp: usize, places: &[Vec<i32>]) {
        let p = &mut self.pps[pp];
        p.count = places[p.word].len() as i64;
        p.next = 0;
        self.next_position(pp, places);
    }

    fn advance(&mut self, pp: usize, places: &[Vec<i32>]) -> bool {
        if !self.next_position(pp, places) {
            return false;
        }
        self.end = self.end.max(self.pps[pp].position);
        true
    }

    /// Place every word on its first match in a new document.
    fn reset(&mut self, places: &[Vec<i32>]) -> bool {
        self.match_length = i32::MAX;
        self.end = i32::MIN;
        for pp in 0..self.pps.len() {
            self.first_position(pp, places);
        }
        // a repeated word starts each of its places one match further on, so
        // no two of them stand on the same place of the document
        for g in 0..self.groups.len() {
            for j in 1..self.groups[g].len() {
                for _ in 0..j {
                    if !self.next_position(self.groups[g][j], places) {
                        self.positioned = false;
                        return false;
                    }
                }
            }
        }
        self.heap.clear();
        for pp in 0..self.pps.len() {
            self.end = self.end.max(self.pps[pp].position);
            let pps = &self.pps;
            self.heap.add(pp, &|a, b| Matcher::less(pps, a, b));
        }
        self.positioned = true;
        true
    }

    fn pop(&mut self) -> usize {
        let pps = &self.pps;
        self.heap.pop(&|a, b| Matcher::less(pps, a, b)).unwrap_or(0)
    }

    fn push(&mut self, pp: usize) {
        let pps = &self.pps;
        self.heap.add(pp, &|a, b| Matcher::less(pps, a, b));
    }

    fn top_position(&self) -> i32 {
        self.heap.top().map(|t| self.pps[t].position).unwrap_or(i32::MAX)
    }

    fn next_match(&mut self, places: &[Vec<i32>]) -> bool {
        if !self.positioned {
            return false;
        }
        let mut pp = self.pop();
        self.match_length = self.end - self.pps[pp].position;
        let mut next = self.top_position();
        while self.advance(pp, places) {
            if !self.groups.is_empty() && !self.advance_repeats(pp, places) {
                break;
            }
            if self.pps[pp].position > next {
                self.push(pp);
                if self.match_length <= self.slop {
                    return true;
                }
                pp = self.pop();
                next = self.top_position();
                self.match_length = self.end - self.pps[pp].position;
            } else {
                let shorter = self.end - self.pps[pp].position;
                if shorter < self.match_length {
                    self.match_length = shorter;
                }
            }
        }
        self.positioned = false;
        self.match_length <= self.slop
    }

    fn sloppy_weight(&self) -> f32 {
        1.0 / (1.0 + self.match_length as f32)
    }

    fn place_of(&self, pp: usize) -> i32 {
        self.pps[pp].position + self.pps[pp].offset
    }

    /// The place of the group standing where `pp` now stands, if any.
    fn collide(&self, pp: usize) -> Option<usize> {
        let at = self.place_of(pp);
        let group = &self.groups[self.pps[pp].group as usize];
        group
            .iter()
            .find(|other| **other != pp && self.place_of(**other) == at)
            .map(|o| self.pps[*o].index)
    }

    fn lesser(&self, a: usize, b: usize) -> usize {
        let (x, y) = (&self.pps[a], &self.pps[b]);
        if x.position < y.position || (x.position == y.position && x.offset < y.offset) {
            a
        } else {
            b
        }
    }

    /// `pp` has moved; where that put it on a place another of its group
    /// holds, the lesser of the two moves on, until none collide.
    fn advance_repeats(&mut self, pp: usize, places: &[Vec<i32>]) -> bool {
        if self.pps[pp].group < 0 {
            return true;
        }
        let g = self.pps[pp].group as usize;
        let size = self.groups[g].len();
        let mut marked = vec![false; size];
        let first = self.pps[pp].index;
        let mut pp = pp;
        while let Some(k) = self.collide(pp) {
            pp = self.lesser(pp, self.groups[g][k]);
            if !self.advance(pp, places) {
                return false;
            }
            if k != first {
                marked[k] = true;
            }
        }
        // the ones that moved while in the queue are taken out and put back
        let mut taken = Vec::new();
        while marked.iter().any(|m| *m) && self.heap.len() > 0 {
            let other = self.pop();
            taken.push(other);
            let p = &self.pps[other];
            if p.group >= 0 && p.index < size && marked[p.index] {
                marked[p.index] = false;
            }
        }
        for other in taken.into_iter().rev() {
            self.push(other);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sloppy frequency of a phrase of distinct words over their places.
    fn freq(words: &[usize], places: &[Vec<i32>], slop: i32) -> f32 {
        let offsets: Vec<usize> = (0..words.len()).collect();
        let mut matcher = Matcher::new(words, &offsets, slop);
        if !matcher.reset(places) || !matcher.next_match(places) {
            return 0.0;
        }
        let mut freq = matcher.sloppy_weight();
        while matcher.next_match(places) {
            freq += matcher.sloppy_weight();
        }
        freq
    }

    #[test]
    fn each_match_counts_by_how_far_the_words_moved() {
        // "notify the supplier in writing" for `notify in writing`
        let places = vec![vec![0], vec![3], vec![4]];
        assert_eq!(freq(&[0, 1, 2], &places, 2), 1.0 / 3.0);
        assert_eq!(freq(&[0, 1, 2], &places, 1), 0.0);
    }

    #[test]
    fn a_repeated_word_needs_two_places() {
        // `the the` against "fox quick brown the quick", which has one
        assert_eq!(freq(&[0, 0], &[vec![3]], 2), 0.0);
        // and against "the the the"
        assert!(freq(&[0, 0], &[vec![0, 1, 2]], 2) > 0.0);
    }
}
