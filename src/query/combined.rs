//! `combined_fields`, scored the way the reference scores it.
//!
//! The query treats the fields it names as one field, and the reference's
//! score says so: BM25F, over a pseudo-field whose statistics are the fields'
//! put together. For each word, the document frequency is the largest any one
//! field has; the index-wide token count is the fields' counts added up, each
//! times its weight; a document's frequency for the word is its frequencies in
//! each field, times their weights, added; and its length is its lengths in
//! each field, times their weights, added -- and then put through the same
//! lossy one-byte encoding every length goes through, so two lengths that
//! round to one byte score alike. Worked by hand on three documents, those
//! rules give 0.5741, 0.4760 and 0.4715, which is what OpenSearch 3.8.0
//! answers.
//!
//! BoostCore keeps what this needs per path of a JSON field -- how many
//! documents hold the path and how many tokens it holds, and each document's
//! length there -- so nothing is estimated.

use super::*;
use boostcore::DocSet;
use boostcore::fieldnorm::FieldNormReader;
use boostcore::postings::Postings;

const K1: f32 = 1.2;
const B: f32 = 0.75;

/// One word of a `combined_fields` query: the same word in each field, with
/// that field's weight.
#[derive(Clone, Debug)]
pub(crate) struct CombinedTerm {
    terms: Vec<(Term, f32)>,
}

impl CombinedTerm {
    pub(crate) fn new(terms: Vec<(Term, f32)>) -> CombinedTerm {
        CombinedTerm { terms }
    }
}

/// The path part of a JSON term, as a fieldnorm reader spells it.
fn path_of(term: &Term) -> Option<Vec<u8>> {
    // a JSON term is its path, a zero byte that ends the path, then the value
    let value = term.serialized_value_bytes();
    let end = value.iter().position(|b| *b == 0u8)?;
    (end > 0).then(|| value[..end].to_vec())
}

impl Query for CombinedTerm {
    fn weight(&self, scoring: EnableScoring<'_>) -> boostcore::Result<Box<dyn Weight>> {
        let stats = match scoring {
            EnableScoring::Enabled { statistics_provider, .. } => {
                let mut doc_count = 0u64;
                let mut tokens = 0f64;
                let mut doc_freq = 0u64;
                for (term, weight) in &self.terms {
                    let (docs, held) = match statistics_provider.path_statistics(term) {
                        Some(s) => s,
                        None => (
                            statistics_provider.total_num_docs()?,
                            statistics_provider.total_num_tokens(term.field())?,
                        ),
                    };
                    doc_count = doc_count.max(docs);
                    tokens += f64::from(*weight) * held as f64;
                    doc_freq = doc_freq.max(statistics_provider.doc_freq(term)?);
                }
                if doc_count == 0 {
                    None
                } else {
                    let idf = (1.0
                        + ((doc_count - doc_freq.min(doc_count)) as f64 + 0.5)
                            / (doc_freq as f64 + 0.5))
                        .ln() as f32;
                    let average = (tokens / doc_count as f64) as f32;
                    Some((idf, average))
                }
            }
            EnableScoring::Disabled { .. } => None,
        };
        Ok(Box::new(CombinedTermWeight { terms: self.terms.clone(), stats }))
    }
}

struct CombinedTermWeight {
    terms: Vec<(Term, f32)>,
    /// the word's idf over the pseudo-field, and the pseudo-field's average
    /// length; none when scores were not asked for
    stats: Option<(f32, f32)>,
}

impl CombinedTermWeight {
    /// Each matching document of the segment with its weighted frequency and
    /// its length over the pseudo-field.
    fn found(
        &self,
        reader: &boostcore::SegmentReader,
    ) -> boostcore::Result<Vec<(boostcore::DocId, f32, f32)>> {
        let mut freqs: std::collections::BTreeMap<boostcore::DocId, f32> = Default::default();
        let mut norms: Vec<(FieldNormReader, f32)> = Vec::new();
        for (term, weight) in &self.terms {
            let inverted = reader.inverted_index(term.field())?;
            if let Some(mut postings) =
                inverted.read_postings(term, IndexRecordOption::WithFreqs)?
            {
                while postings.doc() != boostcore::TERMINATED {
                    *freqs.entry(postings.doc()).or_default() +=
                        weight * postings.term_freq() as f32;
                    postings.advance();
                }
            }
            // the length of this path only: a segment where no document holds
            // the path has no norms for it, and contributes no length -- the
            // whole JSON field's length would be every field of the document
            let norm = match path_of(term) {
                Some(path) => reader.fieldnorms_readers().get_json_path(term.field(), &path)?,
                None => reader.fieldnorms_readers().get_field(term.field())?,
            };
            if let Some(norm) = norm {
                norms.push((norm, *weight));
            }
        }
        Ok(freqs
            .into_iter()
            .map(|(doc, freq)| {
                let length: f32 = norms
                    .iter()
                    .map(|(n, w)| w * FieldNormReader::id_to_fieldnorm(n.fieldnorm_id(doc)) as f32)
                    .sum();
                // the pseudo-field's length goes through the one-byte encoding
                // like any other length
                let encoded = FieldNormReader::fieldnorm_to_id(length as u32);
                (doc, freq, FieldNormReader::id_to_fieldnorm(encoded) as f32)
            })
            .collect())
    }
}

fn bm25f(idf: f32, average: f32, freq: f32, length: f32) -> f32 {
    let norm = K1 * ((1.0 - B) + B * length / average);
    idf * freq / (freq + norm)
}

impl Weight for CombinedTermWeight {
    fn scorer(
        &self,
        reader: &boostcore::SegmentReader,
        boost: boostcore::Score,
    ) -> boostcore::Result<Box<dyn boostcore::query::Scorer>> {
        let found = self.found(reader)?;
        let Some((idf, average)) = self.stats else {
            let docs = found.into_iter().map(|(doc, _, _)| doc).collect();
            return Ok(Box::new(boostcore::query::ConstScorer::new(
                crate::query::spans::KeptDocs::new(docs),
                boost,
            )));
        };
        let scored = found
            .into_iter()
            .map(|(doc, freq, length)| (doc, boost * bm25f(idf, average, freq, length)))
            .collect();
        Ok(Box::new(crate::query::spans::ScoredDocs::new(scored)))
    }

    fn explain(
        &self,
        reader: &boostcore::SegmentReader,
        doc: boostcore::DocId,
    ) -> boostcore::Result<boostcore::query::Explanation> {
        let Some((_, freq, length)) = self.found(reader)?.into_iter().find(|(d, _, _)| *d == doc)
        else {
            return Err(boostcore::TantivyError::InvalidArgument(
                "document does not match the combined_fields term".to_string(),
            ));
        };
        let Some((idf, average)) = self.stats else {
            return Ok(boostcore::query::Explanation::new("combined_fields", 1.0));
        };
        let mut e = boostcore::query::Explanation::new(
            "combined_fields BM25F",
            bm25f(idf, average, freq, length),
        );
        e.add_const("idf", idf);
        e.add_const("freq, weighted over the fields", freq);
        e.add_const("length, weighted over the fields", length);
        e.add_const("average length of the combined field", average);
        Ok(e)
    }
}
