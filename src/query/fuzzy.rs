//! A fuzzy term, scored the way OpenSearch scores one.
//!
//! OpenSearch expands a fuzzy term into the indexed words within reach, keeps
//! the nearest fifty, and scores each as an ordinary term -- BM25 -- weighed
//! by how near it is: one less the edits over the shorter of the two words.
//! The document frequencies are blended to the largest among them, so a rare
//! misspelling does not outscore the word it stands for. Every word within
//! reach had scored the same here, however near it was.
use std::collections::{BTreeMap, HashSet};
use velocore::Searcher;
use velocore::query::{
    Bm25StatisticsProvider, BooleanQuery, BoostQuery, EmptyQuery, EnableScoring, FuzzyTermQuery,
    Occur, Query, TermQuery, Weight,
};
use velocore::schema::{Field, IndexRecordOption, Term};

#[derive(Clone, Debug)]
pub(crate) struct ScoredFuzzy {
    /// the word as written, under its path
    term: Term,
    text: String,
    distance: u8,
    transpositions: bool,
    prefix_length: usize,
    max_expansions: usize,
}

impl ScoredFuzzy {
    pub(crate) fn new(term: Term, text: &str, distance: u8, transpositions: bool) -> Self {
        ScoredFuzzy {
            term,
            text: text.to_string(),
            distance,
            transpositions,
            prefix_length: 0,
            max_expansions: 50,
        }
    }

    pub(crate) fn prefix_length(mut self, n: usize) -> Self {
        self.prefix_length = n;
        self
    }

    pub(crate) fn max_expansions(mut self, n: usize) -> Self {
        self.max_expansions = n.max(1);
        self
    }

    /// The indexed words within reach, nearest first, without their weights:
    /// what a `span_multi` over a fuzzy term rewrites into.
    pub(crate) fn words(&self, searcher: &Searcher) -> velocore::Result<Vec<Term>> {
        Ok(self.expand(searcher)?.into_iter().map(|(term, _)| term).collect())
    }

    /// The indexed words within reach, with the weight each is given.
    fn expand(&self, searcher: &Searcher) -> velocore::Result<Vec<(Term, f32)>> {
        let value = self.term.serialized_value_bytes();
        let head_len = value.len().saturating_sub(self.text.len());
        // the words that share the path, and the fixed prefix if one is asked
        let fixed: String = self.text.chars().take(self.prefix_length).collect();
        let mut low = value[..head_len].to_vec();
        low.extend_from_slice(fixed.as_bytes());
        let high = prefix_end(&low);
        let want: Vec<char> = self.text.chars().collect();
        let mut found: BTreeMap<String, usize> = BTreeMap::new();
        for reader in searcher.segment_readers() {
            let inverted = reader.inverted_index(self.term.field())?;
            let mut range = inverted.terms().range().ge(&low);
            if let Some(high) = &high {
                range = range.lt(high);
            }
            let mut stream = range.into_stream()?;
            while stream.advance() {
                let Ok(word) = std::str::from_utf8(&stream.key()[head_len..]) else { continue };
                if found.contains_key(word) {
                    continue;
                }
                let got: Vec<char> = word.chars().collect();
                if got.len().abs_diff(want.len()) > self.distance as usize {
                    continue;
                }
                if let Some(d) = edits(&want, &got, self.distance as usize, self.transpositions) {
                    found.insert(word.to_string(), d);
                }
            }
        }
        let mut weighed: Vec<(String, f32)> = found
            .into_iter()
            .map(|(word, d)| {
                let shorter = word.chars().count().min(want.len()).max(1) as f32;
                let boost = if d == 0 { 1.0 } else { (1.0 - d as f32 / shorter).max(0.0) };
                (word, boost)
            })
            .collect();
        weighed.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        weighed.truncate(self.max_expansions);
        Ok(weighed
            .into_iter()
            .map(|(word, boost)| {
                let mut term = self.term.clone();
                term.truncate_value_bytes(head_len);
                term.append_bytes(word.as_bytes());
                (term, boost)
            })
            .collect())
    }
}

/// The statistics of the index, with every expanded word given the largest
/// document frequency among them.
struct Blended<'a> {
    inner: &'a dyn Bm25StatisticsProvider,
    terms: HashSet<Term>,
    doc_freq: u64,
}

impl Bm25StatisticsProvider for Blended<'_> {
    fn total_num_tokens(&self, field: Field) -> velocore::Result<u64> {
        self.inner.total_num_tokens(field)
    }

    fn total_num_docs(&self) -> velocore::Result<u64> {
        self.inner.total_num_docs()
    }

    fn doc_freq(&self, term: &Term) -> velocore::Result<u64> {
        if self.terms.contains(term) { Ok(self.doc_freq) } else { self.inner.doc_freq(term) }
    }
}

impl Query for ScoredFuzzy {
    fn weight(&self, scoring: EnableScoring<'_>) -> velocore::Result<Box<dyn Weight>> {
        let Some(searcher) = scoring.searcher() else {
            // with no index to read the words from, the plain automaton
            return FuzzyTermQuery::new(self.term.clone(), self.distance, self.transpositions)
                .weight(scoring);
        };
        let expanded = self.expand(searcher)?;
        if expanded.is_empty() {
            return EmptyQuery.weight(scoring);
        }
        let clauses: Vec<(Occur, Box<dyn Query>)> = expanded
            .iter()
            .map(|(term, boost)| {
                let exact = Box::new(TermQuery::new(term.clone(), IndexRecordOption::WithFreqs));
                (Occur::Should, Box::new(BoostQuery::new(exact, *boost)) as Box<dyn Query>)
            })
            .collect();
        let query = BooleanQuery::new(clauses);
        match scoring {
            EnableScoring::Enabled { searcher, statistics_provider } => {
                let mut doc_freq = 0;
                for (term, _) in &expanded {
                    doc_freq = doc_freq.max(statistics_provider.doc_freq(term)?);
                }
                let blended = Blended {
                    inner: statistics_provider,
                    terms: expanded.iter().map(|(t, _)| t.clone()).collect(),
                    doc_freq,
                };
                query.weight(EnableScoring::enabled_from_statistics_provider(&blended, searcher))
            }
            other => query.weight(other),
        }
    }
}

/// The first key past every key that starts with `prefix`.
fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < u8::MAX {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

/// Edits between two words, counting a swap of neighbours as one when asked,
/// or nothing if there are more than `most`.
fn edits(a: &[char], b: &[char], most: usize, transpositions: bool) -> Option<usize> {
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if transpositions && i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    (d[n][m] <= most).then_some(d[n][m])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(a: &str, b: &str, t: bool) -> Option<usize> {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        edits(&a, &b, 2, t)
    }

    #[test]
    fn edits_count_as_lucene_counts_them() {
        assert_eq!(e("quikc", "quick", true), Some(1));
        assert_eq!(e("quikc", "quick", false), Some(2));
        assert_eq!(e("brwn", "brown", true), Some(1));
        assert_eq!(e("brwn", "bear", true), None);
        assert_eq!(prefix_end(b"ab\xff"), Some(b"ac".to_vec()));
    }
}
