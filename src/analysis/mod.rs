//! How text becomes the tokens an index holds.
//!
//! OpenSearch calls this an analyzer: a character filter, a tokenizer and a
//! chain of token filters. A field names one, an index may define its own, and
//! a search has to cut the query the same way the document was cut or the two
//! never meet.
//!
//! Everything here is built from what the index settings say, and handed to
//! BoostCore as a `TextAnalyzer` under the name the mapping uses.
//!
//! A chain becomes a `TextAnalyzer` -- the whole of it, tokenizer and filters
//! alike -- so that a document and a query are cut by the same code, and the
//! index can be told which analyzer a path is written with.

mod stem;

use std::collections::HashMap;

use boostcore::tokenizer::{
    AsciiFoldingFilter, Language, NgramTokenizer, RawTokenizer, RegexTokenizer, RemoveLongFilter,
    SimpleTokenizer, Stemmer, TextAnalyzer, Token, TokenStream, Tokenizer, WhitespaceTokenizer,
};
use serde_json::{Value, json};

/// The languages OpenSearch names, as BoostCore knows them.
fn language(name: &str) -> Option<Language> {
    Some(match name.to_ascii_lowercase().as_str() {
        "arabic" => Language::Arabic,
        "danish" => Language::Danish,
        "dutch" => Language::Dutch,
        "english" | "porter" | "porter2" | "light_english" | "minimal_english" => Language::English,
        "finnish" => Language::Finnish,
        "french" | "light_french" | "minimal_french" => Language::French,
        "german" | "light_german" | "minimal_german" => Language::German,
        "greek" => Language::Greek,
        "hungarian" => Language::Hungarian,
        "italian" | "light_italian" => Language::Italian,
        "norwegian" => Language::Norwegian,
        "portuguese" | "light_portuguese" | "minimal_portuguese" => Language::Portuguese,
        "romanian" => Language::Romanian,
        "russian" | "light_russian" => Language::Russian,
        "spanish" | "light_spanish" => Language::Spanish,
        "swedish" => Language::Swedish,
        "tamil" => Language::Tamil,
        "turkish" => Language::Turkish,
        _ => return None,
    })
}

/// A tokenizer, before any filter has seen its tokens.
#[derive(Clone, Debug)]
enum Source {
    /// letters and digits, which is what OpenSearch's `standard` keeps
    Standard,
    /// runs of letters only, and what `simple` and `stop` are built on
    Letter,
    Whitespace,
    /// the whole text, as one token
    Keyword,
    Pattern(String),
    /// the pattern says where a token ends rather than what one looks like
    PatternSplit(String),
    /// runs of letters, lowercased as they are read
    LetterLower,
    /// a word may hold an apostrophe, a dot or a hyphen inside it
    Classic,
    /// what `classic` keeps, and an address or a URL kept whole
    UaxUrlEmail,
    /// every prefix of a path: `a`, `a/b`, `a/b/c`
    PathHierarchy {
        delimiter: char,
        replacement: char,
    },
    /// the characters that end a token, named one by one
    CharGroup(Vec<char>),
    Ngram {
        min: usize,
        max: usize,
        edges: bool,
    },
}

/// One step of a chain, in the order OpenSearch writes them.
#[derive(Clone, Debug)]
pub enum Step {
    Lowercase,
    AsciiFolding,
    Stop(Vec<String>),
    Stem(String),
    Length {
        min: usize,
        max: usize,
    },
    Trim,
    Reverse,
    Unique,
    Truncate(usize),
    Limit(usize),
    /// each token replaced by, or joined with, what it maps to
    Synonym(HashMap<String, Vec<String>>),
    /// sorted, deduplicated and joined back into one token
    Fingerprint,
    /// `l'avion` is the word `avion`: the article written onto the front of it
    /// is not part of it
    Elision,
    /// Greek is lowercased with its accents dropped and its final sigma
    /// written as the letter it is
    GreekLowercase,
    PersianNormalize,
    /// Romanian writes the comma below as a cedilla in older text
    RomanianNormalize,
    /// a digit is a digit, whichever script wrote it
    DecimalDigits,
    /// Chinese, Japanese and Korean are written without spaces, so each pair
    /// of neighbouring characters stands in for a word
    CjkBigram,
}

/// A named analysis chain.
#[derive(Clone, Debug)]
pub struct Chain {
    source: Source,
    steps: Vec<Step>,
}

impl Chain {
    /// A chain out of a tokenizer and the steps to run over it.
    pub fn of(source: Chain, steps: Vec<Step>) -> Chain {
        Chain { source: source.source, steps }
    }
}

impl Chain {
    /// The tokens this chain makes of a text, with where each came from.
    pub fn tokens(&self, text: &str) -> Vec<(String, usize, usize, usize)> {
        let mut out = self.cut(text);
        for step in &self.steps {
            out = apply_here(step, out);
        }
        out
    }

    /// The tokens alone, which is what a query needs.
    pub fn terms(&self, text: &str) -> Vec<String> {
        self.tokens(text).into_iter().map(|(t, _, _, _)| t).collect()
    }

    /// The part of the chain BoostCore can run itself.
    pub fn boostcore_analyzer(&self) -> TextAnalyzer {
        let base = match &self.source {
            Source::Standard => TextAnalyzer::builder(SimpleTokenizer::default()).dynamic(),
            Source::Letter | Source::LetterLower => {
                TextAnalyzer::builder(SimpleTokenizer::default()).dynamic()
            }
            Source::Whitespace => TextAnalyzer::builder(WhitespaceTokenizer::default()).dynamic(),
            Source::Keyword => TextAnalyzer::builder(RawTokenizer::default()).dynamic(),
            // the sources below are cut here rather than by BoostCore; the
            // text arrives whole and `tokens` splits it
            Source::PatternSplit(_)
            | Source::Classic
            | Source::UaxUrlEmail
            | Source::PathHierarchy { .. }
            | Source::CharGroup(_) => TextAnalyzer::builder(RawTokenizer::default()).dynamic(),
            Source::Pattern(p) => match RegexTokenizer::new(p) {
                Ok(t) => TextAnalyzer::builder(t).dynamic(),
                Err(_) => TextAnalyzer::builder(SimpleTokenizer::default()).dynamic(),
            },
            Source::Ngram { min, max, edges } => match NgramTokenizer::new(*min, *max, *edges) {
                Ok(t) => TextAnalyzer::builder(t).dynamic(),
                Err(_) => TextAnalyzer::builder(SimpleTokenizer::default()).dynamic(),
            },
        };
        // a token longer than a term may be is dropped, as it is upstream
        base.filter_dynamic(RemoveLongFilter::limit(255)).build()
    }

    /// The tokens the source alone makes, before a filter has seen them.
    pub fn cut(&self, text: &str) -> Vec<(String, usize, usize, usize)> {
        match &self.source {
            Source::PatternSplit(pattern) => split_on(text, pattern),
            Source::CharGroup(chars) => {
                let ends: Vec<char> = chars.clone();
                runs(text, |c| !ends.contains(&c))
            }
            Source::Classic => classic(text),
            Source::UaxUrlEmail => uax_url_email(text),
            Source::PathHierarchy { delimiter, replacement } => {
                path_hierarchy(text, *delimiter, *replacement)
            }
            _ => {
                let mut analyzer = self.boostcore_analyzer();
                let mut stream = analyzer.token_stream(text);
                let mut out = Vec::new();
                while stream.advance() {
                    let t = stream.token();
                    let text = if matches!(self.source, Source::LetterLower) {
                        t.text.to_lowercase()
                    } else {
                        t.text.clone()
                    };
                    out.push((text, t.position, t.offset_from, t.offset_to));
                }
                out
            }
        }
    }

    /// The whole chain, as an analyzer the index can be handed.
    pub fn analyzer(&self) -> TextAnalyzer {
        TextAnalyzer::builder(ChainTokenizer { chain: self.clone() }).build()
    }
}

/// A whole chain, as something BoostCore can be handed.
///
/// The tokens are those of `Chain::tokens`: the source runs inside BoostCore
/// and the steps here, in the order OpenSearch writes them. Cutting a
/// document and cutting the query that looks for it is then the same code.
#[derive(Clone, Debug)]
pub struct ChainTokenizer {
    chain: Chain,
}

impl Tokenizer for ChainTokenizer {
    type TokenStream<'a> = ChainStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> ChainStream {
        let tokens = self
            .chain
            .tokens(text)
            .into_iter()
            .map(|(text, position, offset_from, offset_to)| Token {
                offset_from,
                offset_to,
                position,
                text,
                position_length: 1,
            })
            .collect();
        ChainStream { tokens, cursor: 0 }
    }
}

/// The tokens of one text, already cut.
pub struct ChainStream {
    tokens: Vec<Token>,
    /// one past the token `token()` returns, so that the first `advance()`
    /// lands on the first token
    cursor: usize,
}

impl TokenStream for ChainStream {
    fn advance(&mut self) -> bool {
        self.cursor += 1;
        self.cursor <= self.tokens.len()
    }

    fn token(&self) -> &Token {
        &self.tokens[self.cursor - 1]
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.cursor - 1]
    }
}

/// The runs of characters a predicate keeps, with where each began.
fn runs(text: &str, keep: impl Fn(char) -> bool) -> Vec<(String, usize, usize, usize)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut from = 0usize;
    for (offset, c) in text.char_indices() {
        if keep(c) {
            if current.is_empty() {
                from = offset;
            }
            current.push(c);
        } else if !current.is_empty() {
            out.push((std::mem::take(&mut current), out.len(), from, offset));
        }
    }
    if !current.is_empty() {
        out.push((current, out.len(), from, text.len()));
    }
    out
}

/// What is between the matches of a pattern.
fn split_on(text: &str, pattern: &str) -> Vec<(String, usize, usize, usize)> {
    let Ok(re) = regex::Regex::new(pattern) else {
        return runs(text, |c| !c.is_whitespace());
    };
    let mut out = Vec::new();
    let mut last = 0usize;
    for m in re.find_iter(text) {
        if m.start() > last {
            out.push((text[last..m.start()].to_string(), out.len(), last, m.start()));
        }
        last = m.end();
    }
    if last < text.len() {
        out.push((text[last..].to_string(), out.len(), last, text.len()));
    }
    out
}

/// A word that may hold an apostrophe, a dot or a hyphen between letters,
/// which is what `classic` keeps whole.
fn classic(text: &str) -> Vec<(String, usize, usize, usize)> {
    let mut out = Vec::new();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].1.is_alphanumeric() {
            i += 1;
            continue;
        }
        let from = chars[i].0;
        let mut end = i;
        while end < chars.len() {
            let c = chars[end].1;
            let joins = matches!(c, '\'' | '.' | '_' | '@' | '&')
                && chars.get(end + 1).map(|(_, n)| n.is_alphanumeric()).unwrap_or(false);
            if c.is_alphanumeric() || joins {
                end += 1;
            } else {
                break;
            }
        }
        let to = chars.get(end).map(|(o, _)| *o).unwrap_or(text.len());
        let mut word = text[from..to].to_string();
        // a possessive is not part of the word, the way the classic tokenizer
        // has always had it
        if let Some(base) = word.strip_suffix("'s").or_else(|| word.strip_suffix("'S")) {
            word = base.to_string();
        }
        out.push((word, out.len(), from, to));
        i = end.max(i + 1);
    }
    out
}

/// What `classic` keeps, and an address or a URL kept whole.
fn uax_url_email(text: &str) -> Vec<(String, usize, usize, usize)> {
    let whole =
        regex::Regex::new(r"(?:[a-zA-Z][a-zA-Z0-9+.-]*://[^\s]+)|(?:[\w.+-]+@[\w-]+(?:\.[\w-]+)+)");
    let Ok(whole) = whole else { return classic(text) };
    let mut out: Vec<(String, usize, usize, usize)> = Vec::new();
    let mut last = 0usize;
    for m in whole.find_iter(text) {
        for (word, _, from, to) in classic(&text[last..m.start()]) {
            out.push((word, out.len(), last + from, last + to));
        }
        out.push((m.as_str().to_string(), out.len(), m.start(), m.end()));
        last = m.end();
    }
    for (word, _, from, to) in classic(&text[last..]) {
        out.push((word, out.len(), last + from, last + to));
    }
    out
}

/// Every prefix of a path, so that a search for a directory finds what is
/// under it.
fn path_hierarchy(
    text: &str,
    delimiter: char,
    replacement: char,
) -> Vec<(String, usize, usize, usize)> {
    let mut out = Vec::new();
    let mut so_far = String::new();
    for part in text.split(delimiter) {
        if part.is_empty() && so_far.is_empty() {
            so_far.push(replacement);
            continue;
        }
        if !so_far.is_empty() && !so_far.ends_with(replacement) {
            so_far.push(replacement);
        }
        so_far.push_str(part);
        out.push((so_far.clone(), 0, 0, so_far.len()));
    }
    out
}

/// Steps BoostCore has no filter for, or where OpenSearch's order differs.
fn apply_here(
    step: &Step,
    tokens: Vec<(String, usize, usize, usize)>,
) -> Vec<(String, usize, usize, usize)> {
    match step {
        Step::Lowercase => {
            tokens.into_iter().map(|(t, p, a, b)| (t.to_lowercase(), p, a, b)).collect()
        }
        Step::AsciiFolding => {
            tokens.into_iter().map(|(t, p, a, b)| (fold_to_ascii(&t), p, a, b)).collect()
        }
        Step::Stop(words) => {
            let set: std::collections::HashSet<String> =
                words.iter().map(|w| w.to_lowercase()).collect();
            tokens.into_iter().filter(|(t, _, _, _)| !set.contains(&t.to_lowercase())).collect()
        }
        Step::Stem(lang) => {
            let word_by_word = |f: &dyn Fn(&str) -> String| {
                tokens.iter().map(|(t, p, a, b)| (f(t), *p, *a, *b)).collect::<Vec<_>>()
            };
            // the languages whose analyzer uses a light stemmer rather than
            // the full algorithm, which is what OpenSearch ships
            match lang.to_ascii_lowercase().as_str() {
                "french" | "french_light" | "light_french" => {
                    return word_by_word(&stem::french_light);
                }
                "portuguese" | "portuguese_light" | "light_portuguese" => {
                    return word_by_word(&stem::portuguese_light);
                }
                "italian" | "italian_light" | "light_italian" => {
                    return word_by_word(&stem::italian_light);
                }
                "greek" => return word_by_word(&stem::greek),
                _ => {}
            }
            // the eighteen languages BoostCore carries an algorithm for
            if let Some(l) = language(lang) {
                let mut analyzer =
                    TextAnalyzer::builder(RawTokenizer::default()).filter(Stemmer::new(l)).build();
                return tokens
                    .into_iter()
                    .map(|(t, p, a, b)| {
                        let stemmed = {
                            let mut s = analyzer.token_stream(&t);
                            if s.advance() { s.token().text.clone() } else { t.clone() }
                        };
                        (stemmed, p, a, b)
                    })
                    .collect();
            }
            match lang.to_ascii_lowercase().as_str() {
                "french_light" | "light_french" => word_by_word(&stem::french_light),
                "portuguese_light" | "light_portuguese" => word_by_word(&stem::portuguese_light),
                "italian_light" | "light_italian" => word_by_word(&stem::italian_light),
                "galician" => word_by_word(&stem::galician),
                "brazilian" => word_by_word(&stem::brazilian),
                "bulgarian" => word_by_word(&stem::bulgarian),
                "latvian" => word_by_word(&stem::latvian),
                "indonesian" => word_by_word(&stem::indonesian),
                "czech" => word_by_word(&stem::czech),
                "bengali" => word_by_word(&stem::bengali),
                "hindi" => word_by_word(&stem::hindi),
                "sorani" => word_by_word(&stem::sorani),
                "armenian" => word_by_word(&stem::armenian),
                "basque" => word_by_word(&stem::basque),
                "catalan" => word_by_word(&stem::catalan),
                "irish" => word_by_word(&stem::irish),
                "lithuanian" => word_by_word(&stem::lithuanian),
                "estonian" => word_by_word(&stem::estonian),
                _ => tokens,
            }
        }
        Step::Length { min, max } => tokens
            .into_iter()
            .filter(|(t, _, _, _)| t.chars().count() >= *min && t.chars().count() <= *max)
            .collect(),
        Step::Trim => tokens
            .into_iter()
            .map(|(t, p, a, b)| (t.trim().to_string(), p, a, b))
            .filter(|(t, _, _, _)| !t.is_empty())
            .collect(),
        Step::Reverse => {
            tokens.into_iter().map(|(t, p, a, b)| (t.chars().rev().collect(), p, a, b)).collect()
        }
        Step::Unique => {
            let mut seen = std::collections::HashSet::new();
            tokens.into_iter().filter(|(t, _, _, _)| seen.insert(t.clone())).collect()
        }
        Step::Truncate(n) => {
            tokens.into_iter().map(|(t, p, a, b)| (t.chars().take(*n).collect(), p, a, b)).collect()
        }
        Step::Limit(n) => tokens.into_iter().take(*n).collect(),
        Step::Synonym(map) => {
            let mut out = Vec::new();
            for (t, p, a, b) in tokens {
                match map.get(&t.to_lowercase()) {
                    Some(alts) => {
                        for alt in alts {
                            out.push((alt.clone(), p, a, b));
                        }
                    }
                    None => out.push((t, p, a, b)),
                }
            }
            out
        }
        Step::Fingerprint => {
            let mut words: Vec<String> = tokens.iter().map(|(t, _, _, _)| t.clone()).collect();
            words.sort();
            words.dedup();
            if words.is_empty() {
                return Vec::new();
            }
            let end = tokens.last().map(|(_, _, _, b)| *b).unwrap_or(0);
            vec![(words.join(" "), 0, 0, end)]
        }
        Step::Elision => tokens
            .into_iter()
            .map(|(t, p, a, b)| {
                let cut = t
                    .split_once('\'')
                    .or_else(|| t.split_once('\u{2019}'))
                    .filter(|(head, _)| {
                        matches!(
                            head.to_lowercase().as_str(),
                            "l" | "d"
                                | "j"
                                | "m"
                                | "n"
                                | "s"
                                | "t"
                                | "c"
                                | "qu"
                                | "jusqu"
                                | "lorsqu"
                                | "puisqu"
                                | "quoiqu"
                                | "dell"
                                | "nell"
                                | "sull"
                                | "all"
                                | "un"
                                | "dall"
                                | "b"
                                | "h"
                        )
                    })
                    .map(|(_, rest)| rest.to_string())
                    .unwrap_or(t);
                (cut, p, a, b)
            })
            .filter(|(t, _, _, _)| !t.is_empty())
            .collect(),
        Step::GreekLowercase => {
            tokens.into_iter().map(|(t, p, a, b)| (stem::greek_lowercase(&t), p, a, b)).collect()
        }
        Step::PersianNormalize => tokens
            .into_iter()
            .map(|(t, p, a, b)| (stem::persian_normalize(&t), p, a, b))
            .filter(|(t, _, _, _)| !t.is_empty())
            .collect(),
        Step::RomanianNormalize => tokens
            .into_iter()
            .map(|(t, p, a, b)| {
                let written: String = t
                    .chars()
                    .map(|c| match c {
                        '\u{015F}' => '\u{0219}',
                        '\u{015E}' => '\u{0218}',
                        '\u{0163}' => '\u{021B}',
                        '\u{0162}' => '\u{021A}',
                        other => other,
                    })
                    .collect();
                (written, p, a, b)
            })
            .collect(),
        Step::DecimalDigits => {
            tokens.into_iter().map(|(t, p, a, b)| (stem::decimal_digits(&t), p, a, b)).collect()
        }
        Step::CjkBigram => {
            let mut out = Vec::new();
            for (t, p, a, b) in tokens {
                let chars: Vec<char> = t.chars().collect();
                // a word written in an alphabet is left as it is
                if chars.len() < 2 || !chars.iter().any(|c| is_cjk(*c)) {
                    out.push((t, p, a, b));
                    continue;
                }
                for (i, pair) in chars.windows(2).enumerate() {
                    out.push((pair.iter().collect::<String>(), p + i, a + i, a + i + 2));
                }
            }
            out
        }
    }
}

/// Whether a character is written in a script that has no spaces between its
/// words.
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{30FF}'   // kana
        | '\u{3400}'..='\u{4DBF}' // the rarer Han
        | '\u{4E00}'..='\u{9FFF}' // Han
        | '\u{AC00}'..='\u{D7AF}' // Hangul
        | '\u{F900}'..='\u{FAFF}'
    )
}

/// Latin letters with a mark, written without it.
fn fold_to_ascii(text: &str) -> String {
    let mut analyzer =
        TextAnalyzer::builder(RawTokenizer::default()).filter(AsciiFoldingFilter).build();
    let mut stream = analyzer.token_stream(text);
    if stream.advance() { stream.token().text.clone() } else { text.to_string() }
}

/// The stop words OpenSearch uses when a filter names a language and no list.
fn stop_words(language: &str) -> Vec<String> {
    let list: &[&str] = match language.to_ascii_lowercase().as_str() {
        "_english_" | "english" => &[
            "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into",
            "is", "it", "no", "not", "of", "on", "or", "such", "that", "the", "their", "then",
            "there", "these", "they", "this", "to", "was", "will", "with",
        ],
        "_french_" | "french" => &[
            "au", "aux", "avec", "ce", "ces", "dans", "de", "des", "du", "elle", "en", "et", "eux",
            "il", "je", "la", "le", "les", "leur", "lui", "ma", "mais", "me", "même", "mes", "moi",
            "mon", "ne", "nos", "notre", "nous", "on", "ou", "par", "pas", "pour", "qu", "que",
            "qui", "sa", "se", "ses", "son", "sur", "ta", "te", "tes", "toi", "ton", "tu", "un",
            "une", "vos", "votre", "vous",
        ],
        "_german_" | "german" => &[
            "aber", "alle", "als", "also", "am", "an", "auch", "auf", "aus", "bei", "bin", "bis",
            "da", "das", "dass", "dem", "den", "der", "des", "die", "doch", "du", "ein", "eine",
            "er", "es", "für", "hat", "ich", "ihr", "im", "in", "ist", "mit", "nicht", "noch",
            "nur", "oder", "sich", "sie", "sind", "über", "um", "und", "von", "vor", "war", "was",
            "wenn", "werden", "wie", "wir", "zu", "zum", "zur",
        ],
        "_spanish_" | "spanish" => &[
            "a", "al", "algo", "como", "con", "de", "del", "el", "en", "entre", "era", "es",
            "esta", "este", "ha", "hay", "la", "las", "le", "lo", "los", "más", "me", "mi", "no",
            "o", "para", "pero", "por", "que", "se", "si", "sin", "sobre", "su", "sus", "también",
            "te", "tu", "un", "una", "uno", "y", "ya",
        ],
        "_italian_" | "italian" => &[
            "a", "ad", "al", "alla", "che", "chi", "come", "con", "da", "del", "della", "di", "e",
            "ed", "il", "in", "la", "le", "lo", "ma", "mi", "ne", "non", "per", "più", "quale",
            "se", "si", "sono", "su", "sul", "una", "uno",
        ],
        "_portuguese_" | "portuguese" => &[
            "a", "ao", "aos", "as", "com", "como", "da", "das", "de", "do", "dos", "e", "em",
            "for", "isso", "já", "mais", "mas", "me", "na", "nas", "no", "nos", "não", "o", "os",
            "ou", "para", "pela", "pelo", "por", "que", "se", "sem", "ser", "seu", "sua", "são",
            "também", "um", "uma",
        ],
        "_russian_" | "russian" => &[
            "а",
            "без",
            "более",
            "бы",
            "был",
            "была",
            "были",
            "было",
            "быть",
            "в",
            "вам",
            "вас",
            "весь",
            "во",
            "вот",
            "все",
            "всего",
            "всех",
            "вы",
            "где",
            "да",
            "даже",
            "для",
            "до",
            "его",
            "ее",
            "если",
            "есть",
            "еще",
            "же",
            "за",
            "здесь",
            "и",
            "из",
            "или",
            "им",
            "их",
            "к",
            "как",
            "ко",
            "когда",
            "кто",
            "ли",
            "либо",
            "мне",
            "может",
            "мы",
            "на",
            "надо",
            "наш",
            "не",
            "него",
            "нее",
            "нет",
            "ни",
            "них",
            "но",
            "ну",
            "о",
            "об",
            "они",
            "оно",
            "от",
            "очень",
            "по",
            "под",
            "при",
            "с",
            "со",
            "так",
            "также",
            "такой",
            "там",
            "те",
            "тем",
            "то",
            "того",
            "тоже",
            "той",
            "только",
            "том",
            "ты",
            "у",
            "уже",
            "хотя",
            "чего",
            "чей",
            "чем",
            "что",
            "чтобы",
            "чье",
            "чья",
            "эта",
            "эти",
            "это",
            "я",
        ],
        "_czech_" | "czech" => &[
            "a",
            "s",
            "k",
            "o",
            "i",
            "u",
            "v",
            "z",
            "dnes",
            "cz",
            "t\u{00ED}mto",
            "bude\u{0161}",
            "budem",
            "byli",
            "jse\u{0161}",
            "m\u{016F}j",
            "sv\u{00FD}m",
            "ta",
            "tomto",
            "tohle",
            "tuto",
            "tyto",
            "jej",
            "zda",
            "pro\u{010D}",
            "m\u{00E1}te",
            "tato",
            "kam",
            "tohoto",
            "kdo",
            "kte\u{0159}\u{00ED}",
            "mi",
            "n\u{00E1}m",
            "tom",
            "tomuto",
            "m\u{00ED}t",
            "nic",
            "proto",
            "kterou",
            "byla",
            "toho",
            "proto\u{017E}e",
            "asi",
            "ho",
            "na\u{0161}i",
            "napi\u{0161}te",
            "re",
            "co\u{017E}",
            "t\u{00ED}m",
            "tak\u{017E}e",
            "sv\u{00FD}ch",
            "jej\u{00ED}",
            "svsval",
            "jeho",
            "sv\u{00E9}",
            "pokud",
            "ji\u{017E}",
            "ne\u{017E}",
            "kter\u{00FD}",
            "by",
            "kter\u{00E9}",
            "co",
            "nebo",
            "ten",
            "tak",
            "m\u{00E1}",
            "p\u{0159}i",
            "od",
            "po",
            "jsou",
            "jak",
            "dal\u{0161}\u{00ED}",
            "ale",
            "si",
            "se",
            "ve",
            "to",
            "jako",
            "za",
            "zp\u{011B}t",
            "ze",
            "do",
            "pro",
            "je",
            "na",
            "atd",
            "atp",
            "jakmile",
        ],
        "_persian_" | "persian" => &[
            "\u{0645}\u{06CC}",
            "\u{0648}",
            "\u{062F}\u{0631}",
            "\u{0628}\u{0647}",
            "\u{0627}\u{0632}",
            "\u{06A9}\u{0647}",
            "\u{0627}\u{06CC}\u{0646}",
            "\u{0631}\u{0627}",
            "\u{0628}\u{0627}",
            "\u{0627}\u{0633}\u{062A}",
            "\u{0628}\u{0631}\u{0627}\u{06CC}",
            "\u{06CC}\u{06A9}",
            "\u{062A}\u{0627}",
            "\u{0647}\u{0645}",
            "\u{0622}\u{0646}",
        ],
        "_sorani_" | "sorani" => &[
            "\u{0648}",
            "\u{0644}\u{06D5}",
            "\u{0628}\u{06D5}",
            "\u{0628}\u{06C6}",
            "\u{0644}\u{06D5}\u{06AF}\u{06D5}\u{06B5}",
            "\u{06A9}\u{06D5}",
        ],
        "_greek_" | "greek" => &[
            "\u{03BF}",
            "\u{03B7}",
            "\u{03C4}\u{03BF}",
            "\u{03BA}\u{03B1}\u{03B9}",
            "\u{03BC}\u{03B5}",
            "\u{03C3}\u{03B5}",
            "\u{03B3}\u{03B9}\u{03B1}",
            "\u{03C4}\u{03B7}",
            "\u{03C4}\u{03C9}\u{03BD}",
            "\u{03B1}\u{03C0}\u{03BF}",
        ],
        "_arabic_" | "arabic" => {
            &["من", "في", "على", "و", "أن", "إلى", "عن", "ما", "هذا", "هذه", "التي", "الذي"]
        }
        _ => &[],
    };
    list.iter().map(|s| s.to_string()).collect()
}

/// The analyzers an index can name, whether or not it defined any.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    named: HashMap<String, Chain>,
    /// the tokenizers and filters the index defined, kept so that a request
    /// naming one -- `_analyze` does -- can still be answered
    tokenizers: Value,
    filters: Value,
}

impl Registry {
    /// Read the `analysis` an index's settings define, on top of the built-ins.
    pub fn from_settings(settings: &Value) -> Registry {
        let mut registry = Registry::default();
        let analysis = settings
            .pointer("/index/analysis")
            .or_else(|| settings.pointer("/analysis"))
            .cloned()
            .unwrap_or(Value::Null);
        let filters = analysis.get("filter").cloned().unwrap_or(Value::Null);
        let tokenizers = analysis.get("tokenizer").cloned().unwrap_or(Value::Null);
        let Some(defined) = analysis.get("analyzer").and_then(|a| a.as_object()) else {
            registry.tokenizers = tokenizers;
            registry.filters = filters;
            return registry;
        };
        for (name, spec) in defined {
            if let Some(chain) = build(spec, &tokenizers, &filters) {
                registry.named.insert(name.clone(), chain);
            }
        }
        registry.tokenizers = tokenizers;
        registry.filters = filters;
        registry
    }

    /// A chain described in a request rather than named: a tokenizer, and the
    /// filters over it. The parts may be ones this index defined.
    pub fn custom(&self, spec: &Value) -> Option<Chain> {
        build(spec, &self.tokenizers, &self.filters)
    }

    /// The tokenizer of a request on its own, with no filter over it: what
    /// `_analyze` with `explain` reports before the filters are applied.
    pub fn tokenizer_only(&self, spec: &Value) -> Chain {
        let source = match spec {
            Value::String(name) => tokenizer_source(name, &self.tokenizers),
            other => source_of_spec(other),
        };
        Chain { source, steps: Vec::new() }
    }

    /// One named filter, as the steps it stands for.
    pub fn filter_steps(&self, spec: &Value) -> Vec<Step> {
        match spec {
            Value::String(name) => token_filter(name, &self.filters).unwrap_or_default(),
            other => {
                // a filter described in the request is read the same way one
                // the index defined would be
                let named = json!({ "__inline__": other });
                token_filter("__inline__", &named).unwrap_or_default()
            }
        }
    }

    /// The chain a name stands for: the index's own first, then the built-ins.
    pub fn get(&self, name: &str) -> Option<Chain> {
        self.named.get(name).cloned().or_else(|| builtin(name))
    }

    pub fn names(&self) -> Vec<String> {
        self.named.keys().cloned().collect()
    }
}

/// An analyzer the index defined, out of the parts it named.
fn build(spec: &Value, tokenizers: &Value, filters: &Value) -> Option<Chain> {
    // `{"type": "english"}` names a built-in rather than describing a chain
    if let Some(kind) = spec.get("type").and_then(|t| t.as_str())
        && kind != "custom"
        && let Some(mut chain) = builtin(kind)
    {
        if let Some(list) = spec.get("stopwords") {
            let words = word_list(list);
            chain.steps.retain(|s| !matches!(s, Step::Stop(_)));
            chain.steps.push(Step::Stop(words));
        }
        return Some(chain);
    }
    let named = spec.get("tokenizer").and_then(|t| t.as_str()).unwrap_or("standard");
    let source = tokenizer_source(named, tokenizers);
    let mut steps = Vec::new();
    for step in
        spec.get("filter").into_iter().flat_map(|f| f.as_array().cloned().unwrap_or_default())
    {
        let Some(name) = step.as_str() else { continue };
        if let Some(s) = token_filter(name, filters) {
            steps.extend(s);
        }
    }
    Some(Chain { source, steps })
}

/// A tokenizer by name, defined by the index or built in.
fn tokenizer_source(name: &str, defined: &Value) -> Source {
    if let Some(spec) = defined.get(name) {
        return source_of_spec(spec);
    }
    source_of_name(name)
}

/// A tokenizer described rather than named.
fn source_of_spec(spec: &Value) -> Source {
    let kind = spec.get("type").and_then(|t| t.as_str()).unwrap_or("standard");
    let num =
        |k: &str, d: usize| spec.get(k).and_then(|v| v.as_u64()).map(|v| v as usize).unwrap_or(d);
    let text = |k: &str| spec.get(k).and_then(|v| v.as_str());
    let one = |k: &str, d: char| text(k).and_then(|s| s.chars().next()).unwrap_or(d);
    match kind {
        "pattern" => {
            // `pattern` names what separates the tokens, unless a group is
            // asked for instead
            let pattern = text("pattern").unwrap_or(r"\W+").to_string();
            match spec.get("group").and_then(|g| g.as_i64()) {
                Some(g) if g >= 0 => Source::Pattern(pattern),
                _ => Source::PatternSplit(pattern),
            }
        }
        "simple_pattern" => Source::Pattern(text("pattern").unwrap_or("").to_string()),
        "simple_pattern_split" => Source::PatternSplit(text("pattern").unwrap_or("").to_string()),
        "char_group" => Source::CharGroup(
            spec.get("tokenize_on_chars")
                .and_then(|c| c.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .filter_map(|s| match s {
                            "whitespace" => Some(' '),
                            "letter" | "digit" | "punctuation" | "symbol" => None,
                            other => other.chars().next(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        ),
        "path_hierarchy" | "PathHierarchy" => Source::PathHierarchy {
            delimiter: one("delimiter", '/'),
            replacement: one("replacement", one("delimiter", '/')),
        },
        "ngram" => Source::Ngram { min: num("min_gram", 1), max: num("max_gram", 2), edges: false },
        "edge_ngram" => {
            Source::Ngram { min: num("min_gram", 1), max: num("max_gram", 2), edges: true }
        }
        other => source_of_name(other),
    }
}

/// A tokenizer named rather than described.
fn source_of_name(name: &str) -> Source {
    match name {
        "keyword" => Source::Keyword,
        "whitespace" => Source::Whitespace,
        "letter" => Source::Letter,
        "lowercase" => Source::LetterLower,
        "classic" => Source::Classic,
        "uax_url_email" => Source::UaxUrlEmail,
        "path_hierarchy" | "PathHierarchy" => {
            Source::PathHierarchy { delimiter: '/', replacement: '/' }
        }
        "pattern" => Source::PatternSplit(r"\W+".into()),
        "ngram" => Source::Ngram { min: 1, max: 2, edges: false },
        "edge_ngram" => Source::Ngram { min: 1, max: 2, edges: true },
        _ => Source::Standard,
    }
}

/// A token filter by name, defined by the index or built in.
fn token_filter(name: &str, defined: &Value) -> Option<Vec<Step>> {
    if let Some(spec) = defined.get(name) {
        let kind = spec.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let num = |k: &str, d: usize| {
            spec.get(k).and_then(|v| v.as_u64()).map(|v| v as usize).unwrap_or(d)
        };
        return Some(match kind {
            "stop" => vec![Step::Stop(match spec.get("stopwords") {
                Some(list) => word_list(list),
                None => stop_words("_english_"),
            })],
            "stemmer" | "snowball" => vec![Step::Stem(
                spec.get("language")
                    .or_else(|| spec.get("name"))
                    .and_then(|l| l.as_str())
                    .unwrap_or("english")
                    .to_string(),
            )],
            "length" => vec![Step::Length { min: num("min", 0), max: num("max", usize::MAX) }],
            "truncate" => vec![Step::Truncate(num("length", 10))],
            "limit" => vec![Step::Limit(num("max_token_count", 1))],
            "synonym" | "synonym_graph" => vec![Step::Synonym(synonyms(spec))],
            "lowercase" => vec![Step::Lowercase],
            "uppercase" => vec![Step::Lowercase],
            "asciifolding" => vec![Step::AsciiFolding],
            "trim" => vec![Step::Trim],
            "reverse" => vec![Step::Reverse],
            "unique" => vec![Step::Unique],
            _ => return None,
        });
    }
    Some(match name {
        "lowercase" => vec![Step::Lowercase],
        "asciifolding" => vec![Step::AsciiFolding],
        "trim" => vec![Step::Trim],
        "reverse" => vec![Step::Reverse],
        "unique" => vec![Step::Unique],
        "stop" => vec![Step::Stop(stop_words("_english_"))],
        "porter_stem" | "kstem" | "snowball" => vec![Step::Stem("english".into())],
        "fingerprint" => vec![Step::Fingerprint],
        _ => return None,
    })
}

fn word_list(list: &Value) -> Vec<String> {
    match list {
        Value::String(s) => stop_words(s),
        Value::Array(a) => a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
        _ => Vec::new(),
    }
}

/// `"a, b => c"` and `"a, b"`, which is how synonyms are written.
fn synonyms(spec: &Value) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    let lines = spec.get("synonyms").and_then(|s| s.as_array()).cloned().unwrap_or_default();
    for line in lines.iter().filter_map(|l| l.as_str()) {
        match line.split_once("=>") {
            Some((from, to)) => {
                let targets: Vec<String> = to
                    .split(',')
                    .map(|t| t.trim().to_lowercase())
                    .filter(|t| !t.is_empty())
                    .collect();
                for word in from.split(',').map(|w| w.trim().to_lowercase()) {
                    if !word.is_empty() {
                        map.insert(word, targets.clone());
                    }
                }
            }
            None => {
                let group: Vec<String> = line
                    .split(',')
                    .map(|w| w.trim().to_lowercase())
                    .filter(|w| !w.is_empty())
                    .collect();
                for word in &group {
                    map.insert(word.clone(), group.clone());
                }
            }
        }
    }
    map
}

/// The analyzers OpenSearch has without being told about them.
pub fn builtin(name: &str) -> Option<Chain> {
    // what a language analyzer is: cut into words, lowercased, its own stop
    // words dropped, and what is left cut down to its stem
    let lang = |l: &str| Chain {
        source: Source::Standard,
        steps: vec![Step::Lowercase, Step::Stop(stop_words(l)), Step::Stem(l.to_string())],
    };
    // the languages whose stemmer wants the word written one way first
    let normalized = |l: &str, first: Step| Chain {
        source: Source::Standard,
        steps: vec![Step::Lowercase, first, Step::Stop(stop_words(l)), Step::Stem(l.to_string())],
    };
    Some(match name {
        "standard" | "default" => Chain { source: Source::Standard, steps: vec![Step::Lowercase] },
        "simple" => Chain { source: Source::Letter, steps: vec![Step::Lowercase] },
        "whitespace" => Chain { source: Source::Whitespace, steps: vec![] },
        "stop" => Chain {
            source: Source::Letter,
            steps: vec![Step::Lowercase, Step::Stop(stop_words("_english_"))],
        },
        "keyword" | "raw" => Chain { source: Source::Keyword, steps: vec![] },
        "pattern" => Chain { source: Source::Pattern(r"\w+".into()), steps: vec![Step::Lowercase] },
        "fingerprint" => Chain {
            source: Source::Standard,
            steps: vec![Step::Lowercase, Step::AsciiFolding, Step::Fingerprint],
        },
        "en_stem" => lang("english"),
        // a Snowball analyzer is the English one under the name of the
        // algorithm it runs
        "snowball" => lang("english"),
        // what OpenSearch keeps for indices made long ago: words and stop
        // words, and no stemming at all
        "chinese" => Chain {
            source: Source::Standard,
            steps: vec![Step::Lowercase, Step::Stop(stop_words("_english_"))],
        },
        // Chinese, Japanese and Korean are not written with spaces between
        // words, so a pair of characters stands in for one
        "cjk" => Chain { source: Source::Standard, steps: vec![Step::Lowercase, Step::CjkBigram] },
        // the languages whose stemmer is a light one, or wants the word
        // written its way first
        "french" => normalized("french", Step::Elision),
        "italian" => normalized("italian", Step::Elision),
        "irish" => normalized("irish", Step::Elision),
        "catalan" => normalized("catalan", Step::Elision),
        "greek" => Chain {
            source: Source::Standard,
            steps: vec![
                Step::GreekLowercase,
                Step::Stop(stop_words("greek")),
                Step::Stem("greek".into()),
            ],
        },
        "persian" => Chain {
            source: Source::Standard,
            steps: vec![Step::Lowercase, Step::PersianNormalize, Step::Stop(stop_words("persian"))],
        },
        "thai" => {
            Chain { source: Source::Standard, steps: vec![Step::Lowercase, Step::DecimalDigits] }
        }
        "sorani" => Chain {
            source: Source::Standard,
            steps: vec![
                Step::Lowercase,
                Step::Stop(stop_words("sorani")),
                Step::Stem("sorani".into()),
            ],
        },
        "romanian" => normalized("romanian", Step::RomanianNormalize),
        other => {
            // a language with a light stemmer of its own, or one BoostCore has
            // an algorithm for
            if !KNOWN_LANGUAGES.contains(&other) && language(other).is_none() {
                return None;
            }
            lang(other)
        }
    })
}

/// The languages named by an analyzer, beyond the ones BoostCore stems.
const KNOWN_LANGUAGES: &[&str] = &[
    "armenian",
    "basque",
    "bengali",
    "brazilian",
    "bulgarian",
    "catalan",
    "czech",
    "estonian",
    "galician",
    "hindi",
    "indonesian",
    "irish",
    "latvian",
    "lithuanian",
    "sorani",
];
