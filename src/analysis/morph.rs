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

/// The Japanese reader that breaks compounds apart.
///
/// `関西国際空港` is one word in the dictionary and four in a search box, and
/// the penalties below are the ones Lucene's kuromoji uses to decide when a
/// long word is better read as its parts: a kanji run longer than two
/// characters is charged for each character past that, and anything else for
/// each character past seven.
#[cfg(feature = "cjk")]
fn decomposer() -> &'static Segmenter {
    static CELL: OnceLock<Segmenter> = OnceLock::new();
    CELL.get_or_init(|| {
        let dictionary = load_embedded_dictionary(DictionaryKind::IPADIC)
            .expect("the dictionary is built into this binary");
        Segmenter::new(Mode::Decompose(lindera::mode::Penalty::default()), dictionary, None)
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
    read_with(segmenter(language), language, text)
}

#[cfg(feature = "cjk")]
fn read_with(segmenter: &Segmenter, language: Language, text: &str) -> Vec<Word> {
    let Ok(mut found) = segmenter.segment(Cow::Borrowed(text)) else {
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

/// The words a Japanese text holds, read the way a search box wants them.
///
/// The dictionary's own reading of `関西国際空港` is one word, which is right
/// -- it is the name of an airport. But somebody typing `空港` is looking for
/// it, so the parts are offered as well as the whole: the first part, then
/// the compound standing where the first part stands, then the rest. That is
/// the order Lucene's kuromoji emits them in, and the reason it emits both is
/// that dropping either one loses a search somebody would make.
#[cfg(not(feature = "cjk"))]
pub fn search_words(_text: &str) -> Vec<Word> {
    Vec::new()
}

#[cfg(feature = "cjk")]
pub fn search_words(text: &str) -> Vec<Word> {
    let mut out = Vec::new();
    for word in words(Language::Japanese, text) {
        let within = pieces_of(&word);
        if within.len() < 2 {
            out.push(word);
            continue;
        }
        let mut within = within.into_iter();
        out.push(within.next().expect("at least two"));
        out.push(word);
        out.extend(within);
    }
    out
}

/// A compound broken into the shorter words it is made of, or nothing if it
/// is not one.
///
/// The dictionary holds `関西国際空港` as an entry of its own, so reading the
/// text again cannot find the pieces -- the whole is always the better
/// reading of itself. So the pieces are looked for directly: the shortest
/// sequence of two or more entries that spells the same characters. A run of
/// kanji longer than two characters is where Lucene's kuromoji starts looking
/// too, on the grounds that a shorter one is a word rather than a compound.
#[cfg(feature = "cjk")]
fn pieces_of(word: &Word) -> Vec<Word> {
    const SHORTEST_COMPOUND: usize = 3;
    let letters: Vec<char> = word.text.chars().collect();
    if letters.len() < SHORTEST_COMPOUND || !letters.iter().all(is_kanji) {
        return Vec::new();
    }
    let offsets: Vec<usize> = word
        .text
        .char_indices()
        .map(|(at, _)| at)
        .chain(std::iter::once(word.text.len()))
        .collect();
    // the fewest entries that spell each prefix, and where the last of them
    // began -- a shortest path over the positions between characters
    let n = letters.len();
    let mut best = vec![usize::MAX; n + 1];
    let mut came_from = vec![0usize; n + 1];
    best[0] = 0;
    for to in 1..=n {
        for from in 0..to {
            if best[from] == usize::MAX {
                continue;
            }
            let piece = &word.text[offsets[from]..offsets[to]];
            // the whole word is not one of its own parts
            if piece == word.text || !is_one_entry(piece) {
                continue;
            }
            if best[from] + 1 < best[to] {
                best[to] = best[from] + 1;
                came_from[to] = from;
            }
        }
    }
    if best[n] == usize::MAX || best[n] < 2 {
        return Vec::new();
    }
    let mut cuts = vec![n];
    while *cuts.last().expect("not empty") > 0 {
        cuts.push(came_from[*cuts.last().expect("not empty")]);
    }
    cuts.reverse();
    cuts.windows(2)
        .map(|pair| {
            let piece = &word.text[offsets[pair[0]]..offsets[pair[1]]];
            let mut read = words(Language::Japanese, piece);
            match read.len() {
                1 => {
                    let mut one = read.remove(0);
                    one.from = word.from + offsets[pair[0]];
                    one.to = word.from + offsets[pair[1]];
                    one
                }
                _ => Word {
                    text: piece.to_string(),
                    from: word.from + offsets[pair[0]],
                    to: word.from + offsets[pair[1]],
                    base: None,
                    reading: None,
                    part: None,
                },
            }
        })
        .collect()
}

/// Whether the dictionary holds this exactly, as one word it knows.
#[cfg(feature = "cjk")]
fn is_one_entry(piece: &str) -> bool {
    let read = words(Language::Japanese, piece);
    read.len() == 1 && read[0].text == piece && read[0].part.is_some()
}

/// Whether a character is one of the ones a compound is made of.
#[cfg(feature = "cjk")]
fn is_kanji(c: &char) -> bool {
    matches!(*c, '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}')
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(feature = "cjk")]
    fn a_compound_is_offered_whole_and_in_pieces() {
        let words: Vec<String> = search_words("関西国際空港").into_iter().map(|w| w.text).collect();
        assert_eq!(words, vec!["関西", "関西国際空港", "国際", "空港"]);
    }

    #[test]
    #[cfg(feature = "cjk")]
    fn a_word_that_is_not_a_compound_is_left_alone() {
        for word in ["空港", "寿司", "飲み"] {
            let read: Vec<String> = search_words(word).into_iter().map(|w| w.text).collect();
            assert_eq!(read.len(), 1, "{word} came back as {read:?}");
        }
    }

    #[test]
    #[cfg(feature = "cjk")]
    fn a_sentence_keeps_its_words_in_order() {
        let read: Vec<String> = search_words("寿司がおいしいね").into_iter().map(|w| w.text).collect();
        assert_eq!(read, vec!["寿司", "が", "おいしい", "ね"]);
    }
}

