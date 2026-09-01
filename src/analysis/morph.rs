//! Words in the scripts that do not write spaces between them.
//!
//! Japanese, Korean and Chinese are read with a dictionary: it says where one
//! word ends, what part of speech it is, and -- for a Japanese verb written
//! in one of its many forms -- what the word is when it stands on its own.
//! Lindera carries those dictionaries, and this is how the analyzers named
//! `kuromoji`, `nori` and `smartcn` reach them.

#[cfg(feature = "cjk")]
use std::borrow::Cow;
#[cfg(feature = "cjk")]
use std::sync::OnceLock;

#[cfg(feature = "cjk")]
use lindera::dictionary::{DictionaryKind, load_embedded_dictionary};
#[cfg(feature = "cjk")]
use lindera::mode::Mode;
#[cfg(feature = "cjk")]
use lindera::segmenter::Segmenter;

/// The language a piece of text is read as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    Japanese,
    Korean,
    Chinese,
}

/// One word the dictionary found, and what it says about it.
pub struct Word {
    pub text: String,
    pub from: usize,
    pub to: usize,
    /// the word as it stands on its own -- `飲む` for `飲み`
    pub base: Option<String>,
    /// how it is read, in the script the dictionary writes readings in
    pub reading: Option<String>,
    /// what part of speech it is, as the dictionary names it
    pub part: Option<String>,
}

#[cfg(feature = "cjk")]
#[cfg(feature = "cjk")]
fn segmenter(language: Language) -> &'static Segmenter {
    static JAPANESE: OnceLock<Segmenter> = OnceLock::new();
    static KOREAN: OnceLock<Segmenter> = OnceLock::new();
    static CHINESE: OnceLock<Segmenter> = OnceLock::new();
    let (cell, kind) = match language {
        Language::Japanese => (&JAPANESE, DictionaryKind::IPADIC),
        Language::Korean => (&KOREAN, DictionaryKind::KoDic),
        Language::Chinese => (&CHINESE, DictionaryKind::CcCedict),
    };
    cell.get_or_init(|| {
        let dictionary =
            load_embedded_dictionary(kind).expect("the dictionary is built into this binary");
        Segmenter::new(Mode::Normal, dictionary, None)
    })
}

/// The words a text holds, as the dictionary for that language reads them.
///
/// Built without the dictionaries, there is nothing to read them with, and
/// this says so by finding no words at all rather than guessing at them.
#[cfg(not(feature = "cjk"))]
pub fn words(_language: Language, _text: &str) -> Vec<Word> {
    Vec::new()
}

#[cfg(feature = "cjk")]
#[cfg(not(feature = "cjk"))]
pub fn words(_language: Language, _text: &str) -> Vec<Word> {
    Vec::new()
}

#[cfg(feature = "cjk")]
pub fn words(language: Language, text: &str) -> Vec<Word> {
    let Ok(mut found) = segmenter(language).segment(Cow::Borrowed(text)) else {
        return Vec::new();
    };
    found
        .iter_mut()
        .map(|token| {
            let surface = token.surface.to_string();
            let details: Vec<String> = token.details().iter().map(|d| d.to_string()).collect();
            // the dictionaries write their columns in the order MeCab does:
            // the part of speech first, and the base form and reading last
            let said = |at: usize| {
                details
                    .get(at)
                    .filter(|d| !d.is_empty() && d.as_str() != "*" && d.as_str() != "UNK")
                    .cloned()
            };
            let (base, reading) = match language {
                // ipadic: ..., base form, reading, pronunciation
                Language::Japanese => (said(6), said(7)),
                // ko-dic: the reading is the word itself
                Language::Korean => (None, said(3)),
                Language::Chinese => (None, said(3)),
            };
            Word {
                from: token.byte_start,
                to: token.byte_end,
                base,
                reading,
                part: said(0),
                text: surface,
            }
        })
        .collect()
}

/// Whether a part of speech is one a search has no use for: a particle, a
/// suffix, the ending of a verb.
pub fn is_grammar(part: &str) -> bool {
    part.starts_with("助詞")      // particle
        || part.starts_with("助動詞") // auxiliary verb
        || part.starts_with("記号")   // punctuation
        || part.starts_with('J')      // Korean: the particles
        || part.starts_with('E')      // Korean: the endings
        || part.starts_with("XS") // Korean: the suffixes
}
