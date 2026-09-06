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

mod korean_number;
mod kstem;
mod morph;
mod phone;
mod phonetic;
mod romaji;
mod rslp;
mod snowball;
mod stem;
mod unicode_set;

use std::collections::HashMap;

use boostcore::tokenizer::{
    AsciiFoldingFilter, Language, NgramTokenizer, RawTokenizer, RegexTokenizer, RemoveLongFilter,
    SimpleTokenizer, Stemmer, TextAnalyzer, Token as CoreToken, TokenStream, Tokenizer,
    WhitespaceTokenizer,
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
    /// words the way the Thai analyzer cuts them: Thai by dictionary, and a
    /// Latin word whole across a hyphen, an apostrophe or an underscore
    Thai,
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
    /// A telephone number, cut into everything it may be searched for.
    /// `ngrams` is what separates indexing from searching.
    Phone {
        region: String,
        ngrams: bool,
    },
    /// every prefix of a path: `a`, `a/b`, `a/b/c`
    PathHierarchy {
        delimiter: char,
        replacement: char,
    },
    /// the characters that end a token, named one by one
    CharGroup(Vec<char>),
    /// where one word ends and the next begins, by the Unicode rules and the
    /// dictionaries for the scripts written without spaces
    Icu,
    /// a dictionary that also says what each word is, and what it is when it
    /// stands on its own
    Morph {
        language: morph::Language,
        /// drop the particles and endings a search has no use for
        drop_grammar: bool,
        /// keep each word as it stands on its own
        base_form: bool,
        /// read for a search box: a long compound is offered whole and in
        /// pieces, so that a search for either finds it
        search: bool,
    },
    Ngram {
        min: usize,
        max: usize,
        edges: bool,
    },
}

/// One step of a chain, in the order OpenSearch writes them.
#[derive(Clone, Debug)]
pub enum Step {
    /// the filters inside apply to a token only where the script says so
    Condition {
        script: String,
        inner: Vec<Step>,
    },
    /// a token stays only where the script says so
    Predicate(String),
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
    /// the same word twice in one place is one word; the same word in two
    /// places is two
    UniqueAtPosition,
    /// every token that follows is applied to what the word became as well as
    /// to the word itself
    KeywordRepeat,
    Truncate(usize),
    /// a word written as it sounds, by one of the encoders OpenSearch's
    /// phonetic plugin offers. `replace` says whether the word it was made
    /// from stays beside it
    Phonetic {
        encoder: String,
        replace: bool,
        languages: Vec<String>,
        /// how long a code may be, where the encoder has a length to cap
        max_code_len: Option<usize>,
        /// Beider-Morse reads a name against the tradition it comes from and
        /// the closeness it is asked for
        name_type: String,
        rule_type: String,
    },
    Limit(usize),
    /// each token replaced by, or joined with, what it also means
    Synonym(Vec<SynonymRule>),
    /// sorted, deduplicated and joined back into one token
    Fingerprint(char),
    /// `l'avion` is the word `avion`: the article written onto the front of it
    /// is not part of it
    Elision,
    /// Greek is lowercased with its accents dropped and its final sigma
    /// written as the letter it is
    GreekLowercase,
    /// Irish writes the letter a prefix adds in lower case, and the word
    /// itself in the case it had: `tAthair` is `t-athair`
    IrishLowercase,
    PersianNormalize,
    /// Romanian writes the comma below as a cedilla in older text
    RomanianNormalize,
    /// a digit is a digit, whichever script wrote it
    DecimalDigits,
    /// Chinese, Japanese and Korean are written without spaces, so each pair
    /// of neighbouring characters stands in for a word
    CjkBigram,
    Uppercase,
    /// English, cut the way `kstem` cuts it: gently, and only where the word
    /// stays a word
    KStem,
    /// the tokens a pattern matches, in place of the ones it was given
    PatternCapture {
        patterns: Vec<String>,
        preserve_original: bool,
    },
    PatternReplace {
        pattern: String,
        replacement: String,
    },
    /// only the words named are kept, or only the ones not named
    Keep {
        words: Vec<String>,
        keep: bool,
    },
    /// a token is of a kind -- a word, a number -- and only some are kept
    KeepTypes {
        types: Vec<String>,
        keep: bool,
    },
    /// a word split where the writing changes: `qu1ck` is `qu`, `1` and `ck`
    WordDelimiter {
        catenate: bool,
        on_numerics: bool,
        on_case_change: bool,
        /// whether each part is reported where it was written, or where the
        /// word it came from was
        adjust_offsets: bool,
    },
    /// each word and the one after it, joined, for the words that are too
    /// common to search for on their own
    CommonGrams(Vec<String>),
    /// every run of neighbouring words joined into one token: `the quick fox`
    /// also holds `the quick` and `quick fox`
    Shingle {
        min: usize,
        max: usize,
        unigrams: bool,
        /// where nothing was shingled, the words themselves are the answer
        unigrams_if_none: bool,
        separator: String,
        filler: String,
    },
    /// `John's` is `John`
    Apostrophe,
    /// the `'s` at the end of an English word is not part of it
    Possessive,
    /// the possessive gone, and the dots out of an acronym
    Classic,
    /// the wide characters written narrow
    CjkWidth,
    /// the letters of a script written the one way the index keeps them
    Normalize(&'static str),
    /// `foo^3` is the word `foo`, three times over
    DelimitedTermFreq(char),
    /// `foo|bar` is the word `foo` with `bar` hung off it
    DelimitedPayload(char),
    /// the same token, put through several filters at once
    Multiplexer(Vec<Vec<Step>>),
    /// the paths of a graph pressed flat, so that every token stands one
    /// place after the one before it
    FlattenGraph,
    /// each token carries what kind of token it is, for a term vector to
    /// read back
    TypeAsPayload,
    /// a word named here is left alone by the stemmers after it
    KeywordMarker(Vec<String>),
    /// a word named here is stemmed to what it is told, not what the
    /// algorithm would say
    StemmerOverride(HashMap<String, String>),
    /// the pieces a long word is made of, when they are words themselves
    Decompound(Vec<String>),
    /// every run of `min` to `max` characters inside each token
    NgramTokens {
        min: usize,
        max: usize,
        edges: bool,
    },
    /// the original token kept beside what the filter made of it
    PreserveOriginal(Box<Step>),
    /// the smallest hash of the text in each of many buckets: two texts that
    /// share most of their words share most of their hashes
    MinHash {
        buckets: usize,
        hashes: usize,
    },
    /// the word written the one way Unicode says it is written, in the case
    /// it is compared in: `Ruß` is `russ`. A set, where one is given, says
    /// which characters may be changed and leaves the rest alone.
    IcuNormalize(Option<unicode_set::UnicodeSet>),
    /// the same, and the marks written on the letters dropped as well
    IcuFold(Option<unicode_set::UnicodeSet>),
    /// `nori_number`: a Korean number written in words, as digits
    KoreanNumber,
    /// `icu_collation`: two words a language considers the same at this
    /// strength become the same token, so a search for one finds the other
    Collate {
        strength: Strength,
    },
    /// a word is kept as it stands on its own: `飲み` is `飲む`
    BaseForm(morph::Language),
    /// the parts of speech a search has no use for -- a particle, an ending --
    /// are dropped, or the ones the filter names where it names any
    PartOfSpeech {
        language: morph::Language,
        stoptags: Option<Vec<String>>,
    },
    /// how the word is read, rather than how it is written
    Reading(morph::Language),
    /// `kuromoji_stemmer`: a katakana word long enough to have been written
    /// with a long mark loses it, so `サーバー` and `サーバ` are one word
    KatakanaStem {
        minimum: usize,
    },
    /// `kuromoji_completion`: the word, and its reading written in the Latin
    /// alphabet in both of the systems that write it differently
    Completion {
        index: bool,
    },
}

impl Step {
    /// Whether this step cuts a word down to its stem.
    ///
    /// A `keyword_repeat` before it means the word itself is kept as well.
    fn stems(&self) -> bool {
        matches!(self, Step::Stem(_) | Step::KStem | Step::StemmerOverride(_) | Step::Decompound(_))
    }
}

/// One token: its text, the place it stands in, where it came from in the
/// text, and how many places it spans -- a word that stands for two words,
/// as a synonym may, spans two.
pub type Token = (String, usize, usize, usize, usize);

/// A named analysis chain.
#[derive(Clone, Debug)]
pub struct Chain {
    /// what is done to the text before a tokenizer sees it
    pre: Vec<CharFilter>,
    source: Source,
    steps: Vec<Step>,
    /// the field is `annotated_text`: `[shown](value)` is markup, so the
    /// markup comes off before the text is cut, and each annotation is a
    /// token of its own standing where its span begins
    annotated: bool,
}

/// The name a field's analyzer is registered under once the annotation markup
/// has to come off first. It is not a name anybody can write in a mapping --
/// `#` cannot appear in one -- so it cannot collide with an analyzer somebody
/// defined.
pub fn annotated_name(base: &str) -> String {
    format!("#annotated#{base}")
}

/// The analyzer such a name stands for, if it is one.
pub fn annotated_base(name: &str) -> Option<&str> {
    name.strip_prefix("#annotated#")
}

/// A change made to the text itself, before it is cut into tokens.
#[derive(Clone, Debug)]
pub enum CharFilter {
    /// the tags taken out, and the ones named left where they are
    HtmlStrip(Vec<String>),
    /// `ph => f`, applied where it is found
    Mapping(Vec<(String, String)>),
    Replace {
        pattern: String,
        replacement: String,
    },
    IcuNormalize(Option<unicode_set::UnicodeSet>),
}

impl CharFilter {
    /// The text this filter makes of the text it is given, and where each
    /// byte of it came from: an entry per byte of the output, and one more
    /// for its end, each naming the byte of the input it stands for.
    ///
    /// A token cut out of the filtered text is reported where it stood in
    /// the text the caller sent, which is what the map is for.
    pub fn applied_mapped(&self, text: &str) -> (String, Vec<usize>) {
        match self {
            CharFilter::Mapping(rules) => {
                let mut out = String::with_capacity(text.len());
                let mut map: Vec<usize> = Vec::with_capacity(text.len() + 1);
                let chars: Vec<(usize, char)> = text.char_indices().collect();
                let mut i = 0;
                'outer: while i < chars.len() {
                    for (from, to) in rules {
                        let width = from.chars().count();
                        if width > 0 && i + width <= chars.len() {
                            let here: String =
                                chars[i..i + width].iter().map(|(_, c)| *c).collect();
                            if here == *from {
                                // what the rule wrote stands for the whole
                                // of what it replaced
                                let at = chars[i].0;
                                for _ in 0..to.len() {
                                    map.push(at);
                                }
                                out.push_str(to);
                                i += width;
                                continue 'outer;
                            }
                        }
                    }
                    let (at, c) = chars[i];
                    for _ in 0..c.len_utf8() {
                        map.push(at);
                    }
                    out.push(c);
                    i += 1;
                }
                map.push(text.len());
                (out, map)
            }
            CharFilter::Replace { pattern, replacement } => match regex::Regex::new(pattern) {
                Ok(re) => {
                    let mut out = String::with_capacity(text.len());
                    let mut map: Vec<usize> = Vec::with_capacity(text.len() + 1);
                    let mut last = 0usize;
                    for m in re.find_iter(text) {
                        for b in last..m.start() {
                            map.push(b);
                        }
                        out.push_str(&text[last..m.start()]);
                        let written = re.replace(m.as_str(), replacement.as_str()).into_owned();
                        for _ in 0..written.len() {
                            map.push(m.start());
                        }
                        out.push_str(&written);
                        last = m.end();
                    }
                    for b in last..text.len() {
                        map.push(b);
                    }
                    out.push_str(&text[last..]);
                    map.push(text.len());
                    (out, map)
                }
                Err(_) => (text.to_string(), (0..=text.len()).collect()),
            },
            // a filter that rewrites the text wholesale is mapped end to end:
            // the same byte where the lengths agree, and the last byte of
            // the input for the end of the output where they do not
            other => {
                let out = other.applied(text);
                let map: Vec<usize> = match out.len() == text.len() {
                    true => (0..=text.len()).collect(),
                    false => (0..=out.len())
                        .map(|b| if out.is_empty() { 0 } else { (b * text.len()) / out.len() })
                        .collect(),
                };
                (out, map)
            }
        }
    }

    /// The text this filter makes of the text it is given.
    pub fn applied(&self, text: &str) -> String {
        match self {
            CharFilter::HtmlStrip(kept) => strip_html(text, kept),
            CharFilter::Mapping(rules) => {
                let mut out = String::with_capacity(text.len());
                let bytes: Vec<char> = text.chars().collect();
                let mut i = 0;
                'outer: while i < bytes.len() {
                    for (from, to) in rules {
                        let width = from.chars().count();
                        if width > 0 && i + width <= bytes.len() {
                            let here: String = bytes[i..i + width].iter().collect();
                            if here == *from {
                                out.push_str(to);
                                i += width;
                                continue 'outer;
                            }
                        }
                    }
                    out.push(bytes[i]);
                    i += 1;
                }
                out
            }
            CharFilter::Replace { pattern, replacement } => match regex::Regex::new(pattern) {
                Ok(re) => re.replace_all(text, replacement.as_str()).into_owned(),
                Err(_) => text.to_string(),
            },
            CharFilter::IcuNormalize(set) => match set {
                Some(set) => unicode_set::within(text, set, |c| icu_normalize(c)),
                None => icu_normalize(text),
            },
        }
    }
}

/// The text of an HTML document, with the tags taken out.
fn strip_html(text: &str, kept: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '<' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let Some(close) = chars[i..].iter().position(|c| *c == '>').map(|p| i + p) else {
            out.push(chars[i]);
            i += 1;
            continue;
        };
        let tag: String = chars[i + 1..close].iter().collect();
        let name = tag.trim_start_matches('/').split_whitespace().next().unwrap_or("").to_string();
        // a tag the request asked to keep is left where it stands
        if kept.iter().any(|k| k.eq_ignore_ascii_case(&name)) {
            out.extend(&chars[i..=close]);
        } else {
            // a tag is a break in the text, which is a line of its own
            out.push('\n');
        }
        i = close + 1;
    }
    out
}

impl Chain {
    /// A chain out of a tokenizer and the steps to run over it.
    pub fn of(source: Chain, steps: Vec<Step>) -> Chain {
        Chain { pre: source.pre, source: source.source, steps, annotated: false }
    }

    /// The text as the char filters leave it, before it is cut.
    /// A chain with char filters put in front of it.
    pub fn filtered(pre: Vec<CharFilter>, chain: Chain) -> Chain {
        Chain { pre, source: chain.source, steps: chain.steps, annotated: false }
    }
}

impl Chain {
    /// The synonym rules written the way the chain in front of them would
    /// write them.
    ///
    /// A rule is text, and Lucene reads it with the tokenizer and the filters
    /// that stand before the synonym filter: a rule over `foobar` in a chain
    /// that cuts into trigrams is a rule over `foo oob oba bar`.
    fn cut_synonyms(&mut self) {
        for at in 0..self.steps.len() {
            let Step::Synonym(rules) = &self.steps[at] else { continue };
            let before = Chain {
                pre: self.pre.clone(),
                source: self.source.clone(),
                steps: self.steps[..at].to_vec(),
                annotated: false,
            };
            let cut = |words: &[String]| -> Vec<String> {
                let text = words.join(" ");
                let terms = before.terms(&text);
                if terms.is_empty() { words.to_vec() } else { terms }
            };
            let rewritten: Vec<SynonymRule> = rules
                .iter()
                .map(|rule| SynonymRule {
                    phrase: cut(&rule.phrase),
                    alternatives: rule.alternatives.iter().map(|a| cut(a)).collect(),
                    keep_original: rule.keep_original,
                    alternatives_first: rule.alternatives_first,
                    graph: rule.graph,
                })
                .collect();
            self.steps[at] = Step::Synonym(rewritten);
        }
    }

    /// Whether the chain hangs each token's kind on it as a payload.
    pub fn carries_type_payload(&self) -> bool {
        self.steps.iter().any(|s| matches!(s, Step::TypeAsPayload))
    }

    /// Whether the chain cuts each word into pieces after cutting the words.
    ///
    /// The pieces carry the offsets of the word they came from, so a match on
    /// a piece is a match on the whole word.
    pub fn filters_into_ngrams(&self) -> bool {
        self.steps.iter().any(|s| matches!(s, Step::NgramTokens { .. }))
    }

    /// Whether the chain cuts text into pieces of words rather than words.
    ///
    /// A field written that way is matched on the pieces, and a highlighter
    /// marks the pieces inside the words rather than the words.
    pub fn cuts_into_ngrams(&self) -> bool {
        // an ngram filter after a tokenizer still reports the offsets of the
        // whole word it cut, so only a tokenizer that cuts pieces places
        // them inside the words
        matches!(self.source, Source::Ngram { .. })
    }

    /// The tokens this chain makes of a text, with where each came from.
    pub fn tokens(&self, text: &str) -> Vec<Token> {
        if self.annotated {
            return self.annotated_tokens(text);
        }
        let mut out = self.cut(text);
        let mut held = Held {
            // a word a `keyword_marker` names is left as it was written,
            // whatever the stemmers after it would have done to it
            protected: std::collections::HashSet::new(),
            // What a word is depends on the words around it: `가` read on its
            // own is a verb, and `뿌리가 깊은 나무` reads the same `가` as the
            // particle it is. So a dictionary that says what each word is says
            // it while it is reading the text, and a filter downstream reads
            // that rather than asking again about a word standing alone.
            parts: self.parts_of(text),
        };
        for step in &self.steps {
            out = apply_step(step, out, &mut held);
        }
        out
    }

    /// The tokens of an annotated field.
    ///
    /// The markup is not part of the text: `[quick brown fox](entity_3789)`
    /// is the words `quick brown fox` with a thing said about them. So the
    /// text is cut without it, and each annotation is a token standing where
    /// its span begins -- beside the first word of the span rather than after
    /// the last, so that a phrase running through the span is still a phrase.
    fn annotated_tokens(&self, text: &str) -> Vec<Token> {
        let (plain, marks) = crate::search::highlight::without_markup(text);
        let mut plain_chain = self.clone();
        plain_chain.annotated = false;
        let cut = plain_chain.tokens(&plain);
        let mut out: Vec<Token> = Vec::new();
        for (at, token) in cut.iter().enumerate() {
            for mark in &marks {
                // the annotation stands where the first token of its span does
                let starts_here = token.2 >= mark.from
                    && token.2 < mark.to
                    && !cut[..at].iter().any(|t| t.2 >= mark.from && t.2 < mark.to);
                if starts_here {
                    for value in mark.raw.split('&').filter(|v| !v.is_empty()) {
                        out.push((value.to_string(), token.1, mark.from, mark.to, 1));
                    }
                }
            }
            out.push(token.clone());
        }
        out
    }

    /// What the dictionary called each word, by where it stood.
    fn parts_of(&self, text: &str) -> std::collections::HashMap<(usize, usize), String> {
        let Source::Morph { language, search, .. } = &self.source else {
            return std::collections::HashMap::new();
        };
        let read = match (search, language) {
            (true, morph::Language::Japanese) => morph::search_words(text),
            _ => morph::words(*language, text),
        };
        read.into_iter().filter_map(|w| w.part.map(|part| ((w.from, w.to), part))).collect()
    }

    /// The tokens alone, which is what a query needs.
    pub fn terms(&self, text: &str) -> Vec<String> {
        self.tokens(text).into_iter().map(|(t, _, _, _, _)| t).collect()
    }

    /// The part of the chain BoostCore can run itself.
    pub fn boostcore_analyzer(&self) -> TextAnalyzer {
        let base = match &self.source {
            Source::Standard => TextAnalyzer::builder(SimpleTokenizer::default()).dynamic(),
            // `letter` keeps runs of letters and nothing else, so it is cut
            // here rather than by the tokenizer that stands for `standard`
            Source::Letter | Source::LetterLower => {
                TextAnalyzer::builder(RawTokenizer::default()).dynamic()
            }
            Source::Whitespace => TextAnalyzer::builder(WhitespaceTokenizer::default()).dynamic(),
            Source::Keyword => TextAnalyzer::builder(RawTokenizer::default()).dynamic(),
            // the sources below are cut here rather than by BoostCore; the
            // text arrives whole and `tokens` splits it
            Source::PatternSplit(_)
            | Source::Classic
            | Source::UaxUrlEmail
            | Source::PathHierarchy { .. }
            | Source::Icu
            | Source::Thai
            | Source::Morph { .. }
            | Source::CharGroup(_)
            | Source::Phone { .. } => TextAnalyzer::builder(RawTokenizer::default()).dynamic(),
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
    pub fn cut(&self, text: &str) -> Vec<Token> {
        if self.pre.is_empty() {
            return self.cut_prepared(text);
        }
        // each filter maps its output back onto its input, and the maps
        // compose back to the text the caller sent
        let mut held = text.to_string();
        let mut back: Vec<usize> = (0..=text.len()).collect();
        for filter in &self.pre {
            let (out, map) = filter.applied_mapped(&held);
            back = map.iter().map(|at| back[(*at).min(back.len() - 1)]).collect();
            held = out;
        }
        let mut tokens = self.cut_prepared(&held);
        for token in tokens.iter_mut() {
            token.2 = back[token.2.min(back.len() - 1)];
            token.3 = back[token.3.min(back.len() - 1)];
        }
        tokens
    }

    /// The tokens of a text the char filters have already been through.
    fn cut_prepared(&self, text: &str) -> Vec<Token> {
        match &self.source {
            Source::PatternSplit(pattern) => split_on(text, pattern),
            Source::CharGroup(chars) => {
                let ends: Vec<char> = chars.clone();
                runs(text, |c| !ends.contains(&c))
            }
            Source::Classic => classic(text),
            Source::Icu => icu_words(text),
            Source::Thai => thai_words(text),
            Source::Letter => runs(text, |c| c.is_alphabetic()),
            Source::LetterLower => runs(text, |c| c.is_alphabetic())
                .into_iter()
                .map(|(t, p, a, b, l)| (t.to_lowercase(), p, a, b, l))
                .collect(),
            Source::Morph { language, drop_grammar, base_form, search } => {
                // the dictionary says what each word is while it is reading
                // the text; asking again about one word on its own would not
                // give the same answer, so the choice is made here
                let read = match (search, language) {
                    (true, morph::Language::Japanese) => morph::search_words(text),
                    _ => morph::words(*language, text),
                };
                read.into_iter()
                    .filter(|w| {
                        if !drop_grammar {
                            return true;
                        }
                        if w.text.chars().all(|c| !c.is_alphanumeric()) {
                            return false;
                        }
                        w.part.as_deref().map(|p| !morph::is_grammar(p)).unwrap_or(true)
                    })
                    .enumerate()
                    .map(|(i, w)| {
                        let text = match base_form {
                            true => w.base.clone().unwrap_or(w.text),
                            false => w.text,
                        };
                        // Chinese punctuation carries no meaning of its own,
                        // and the sentence it ends could have ended with any
                        // of a dozen marks: they are all one token, so that a
                        // phrase query knows a sentence ended without caring
                        // which mark ended it. This is what Lucene's own
                        // Chinese tokenizer does, and its stop words then
                        // drop it for the analyzer that uses one.
                        let text = match *language == morph::Language::Chinese
                            && !text.is_empty()
                            && text.chars().all(|c| !c.is_alphanumeric())
                        {
                            true => ",".to_string(),
                            false => text,
                        };
                        (text, i, w.from, w.to, 1)
                    })
                    .collect()
            }
            Source::UaxUrlEmail => uax_url_email(text),
            Source::PathHierarchy { delimiter, replacement } => {
                path_hierarchy(text, *delimiter, *replacement)
            }
            Source::Phone { region, ngrams } => phone::tokens(text, region, *ngrams)
                .into_iter()
                .enumerate()
                .map(|(i, t)| (t, i, 0, text.len(), 1))
                .collect(),
            _ => {
                let mut analyzer = self.boostcore_analyzer();
                let mut stream = analyzer.token_stream(text);
                let mut out = Vec::new();
                while stream.advance() {
                    let t = stream.token();
                    out.push((t.text.clone(), t.position, t.offset_from, t.offset_to, 1));
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
            .map(|(text, position, offset_from, offset_to, length)| CoreToken {
                offset_from,
                offset_to,
                position,
                text,
                position_length: length,
            })
            .collect();
        ChainStream { tokens, cursor: 0 }
    }
}

/// The tokens of one text, already cut.
pub struct ChainStream {
    tokens: Vec<CoreToken>,
    /// one past the token `token()` returns, so that the first `advance()`
    /// lands on the first token
    cursor: usize,
}

impl TokenStream for ChainStream {
    fn advance(&mut self) -> bool {
        self.cursor += 1;
        self.cursor <= self.tokens.len()
    }

    fn token(&self) -> &CoreToken {
        &self.tokens[self.cursor - 1]
    }

    fn token_mut(&mut self) -> &mut CoreToken {
        &mut self.tokens[self.cursor - 1]
    }
}

/// The runs of characters a predicate keeps, with where each began.
fn runs(text: &str, keep: impl Fn(char) -> bool) -> Vec<Token> {
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
            out.push((std::mem::take(&mut current), out.len(), from, offset, 1));
        }
    }
    if !current.is_empty() {
        out.push((current, out.len(), from, text.len(), 1));
    }
    out
}

/// What is between the matches of a pattern.
fn split_on(text: &str, pattern: &str) -> Vec<Token> {
    let Ok(re) = regex::Regex::new(pattern) else {
        return runs(text, |c| !c.is_whitespace());
    };
    let mut out = Vec::new();
    let mut last = 0usize;
    for m in re.find_iter(text) {
        if m.start() > last {
            out.push((text[last..m.start()].to_string(), out.len(), last, m.start(), 1));
        }
        last = m.end();
    }
    if last < text.len() {
        out.push((text[last..].to_string(), out.len(), last, text.len(), 1));
    }
    out
}

/// A word that may hold an apostrophe, a dot or a hyphen between letters,
/// which is what `classic` keeps whole.
fn classic(text: &str) -> Vec<Token> {
    classic_with(text, false)
}

/// The same, told whether an underscore is inside a word.
fn classic_with(text: &str, underscore_joins: bool) -> Vec<Token> {
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
            let joins = (matches!(c, '\'' | '.' | '@' | '&') || (underscore_joins && c == '_'))
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
        out.push((word, out.len(), from, to, 1));
        i = end.max(i + 1);
    }
    out
}

/// What `classic` keeps, and an address or a URL kept whole.
fn uax_url_email(text: &str) -> Vec<Token> {
    let whole =
        regex::Regex::new(r"(?:[a-zA-Z][a-zA-Z0-9+.-]*://[^\s]+)|(?:[\w.+-]+@[\w-]+(?:\.[\w-]+)+)");
    let Ok(whole) = whole else { return classic_with(text, true) };
    let mut out: Vec<Token> = Vec::new();
    let mut last = 0usize;
    for m in whole.find_iter(text) {
        for (word, _, from, to, _) in classic_with(&text[last..m.start()], true) {
            out.push((word, out.len(), last + from, last + to, 1));
        }
        out.push((m.as_str().to_string(), out.len(), m.start(), m.end(), 1));
        last = m.end();
    }
    for (word, _, from, to, _) in classic_with(&text[last..], true) {
        out.push((word, out.len(), last + from, last + to, 1));
    }
    out
}

/// Every prefix of a path, so that a search for a directory finds what is
/// under it.
fn path_hierarchy(text: &str, delimiter: char, replacement: char) -> Vec<Token> {
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
        out.push((so_far.clone(), 0, 0, so_far.len(), 1));
    }
    out
}

/// Steps BoostCore has no filter for, or where OpenSearch's order differs.
/// A script over one token at a time, answering whether it holds: the
/// token is `token`, with its term, position, offsets and the rest.
fn token_judge(script: &str) -> Option<Box<dyn Fn(&Token, Option<usize>) -> bool>> {
    let compiled = crate::painless::Script::compile(script).ok()?;
    Some(Box::new(move |tok: &Token, previous: Option<usize>| -> bool {
        use crate::painless::Value as V;
        let (term, position, start, end, length) = tok;
        let increment = match previous {
            Some(p) => position.saturating_sub(p) as i64,
            None => 1,
        };
        let token = V::map(vec![
            (V::str("term"), V::str(term)),
            (V::str("position"), V::Int(*position as i64)),
            (V::str("positionIncrement"), V::Int(increment)),
            (V::str("positionLength"), V::Int((*length).max(1) as i64)),
            (V::str("startOffset"), V::Int(*start as i64)),
            (V::str("endOffset"), V::Int(*end as i64)),
            (V::str("type"), V::str("word")),
            (V::str("keyword"), V::Bool(false)),
        ]);
        let mut runner = crate::painless::contexts::Runner::new(&serde_json::json!({}));
        runner.token = Some(token);
        runner.run(&compiled).ok().and_then(|v| v.truthy()).unwrap_or(false)
    }))
}

fn apply_step(step: &Step, tokens: Vec<Token>, held: &mut Held) -> Vec<Token> {
    let protected = &mut held.protected;
    // the words held back keep the spelling they were written with
    if let Step::KeywordMarker(words) = step {
        protected.extend(words.iter().map(|w| w.to_lowercase()));
        return tokens;
    }
    if let Step::StemmerOverride(told) = step {
        // the word it was told to become is the word it stays
        protected.extend(told.values().map(|t| t.to_lowercase()));
    }
    if matches!(step, Step::Stem(_) | Step::KStem) && !protected.is_empty() {
        let (kept, rest): (Vec<_>, Vec<_>) =
            tokens.into_iter().partition(|(t, _, _, _, _)| protected.contains(&t.to_lowercase()));
        let mut out = apply_step(step, rest, &mut Held::new());
        out.extend(kept);
        out.sort_by_key(|(_, position, _, _, _)| *position);
        return out;
    }
    match step {
        Step::Predicate(script) => {
            let judge = token_judge(script);
            let mut previous: Option<usize> = None;
            tokens
                .into_iter()
                .filter(|tok| {
                    let keep = judge.as_ref().map(|j| j(tok, previous)).unwrap_or(true);
                    previous = Some(tok.1);
                    keep
                })
                .collect()
        }
        Step::Condition { script, inner } => {
            let judge = token_judge(script);
            let mut previous: Option<usize> = None;
            let mut out = Vec::with_capacity(tokens.len());
            for tok in tokens {
                let applies = judge.as_ref().map(|j| j(&tok, previous)).unwrap_or(true);
                previous = Some(tok.1);
                if applies {
                    let mut one = vec![tok];
                    for step in inner {
                        one = apply_step(step, one, held);
                    }
                    out.extend(one);
                } else {
                    out.push(tok);
                }
            }
            out
        }
        Step::Lowercase => {
            tokens.into_iter().map(|(t, p, a, b, l)| (t.to_lowercase(), p, a, b, l)).collect()
        }
        Step::AsciiFolding => {
            tokens.into_iter().map(|(t, p, a, b, l)| (fold_to_ascii(&t), p, a, b, l)).collect()
        }
        Step::Stop(words) => {
            // the words are compared as they are written: `The` is not the
            // stop word `the`, which is how OpenSearch reads them
            let set: std::collections::HashSet<&String> = words.iter().collect();
            tokens.into_iter().filter(|(t, _, _, _, _)| !set.contains(t)).collect()
        }
        Step::Stem(lang) => {
            let word_by_word = |f: &dyn Fn(&str) -> String| {
                tokens.iter().map(|(t, p, a, b, l)| (f(t), *p, *a, *b, *l)).collect::<Vec<_>>()
            };
            // the languages whose analyzer uses a light stemmer rather than
            // the full algorithm, which is what OpenSearch ships
            // an analyzer stems its language lightly, where OpenSearch does;
            // the filter named `<language>_stem` runs the full algorithm
            match lang.to_ascii_lowercase().as_str() {
                "french_light" | "light_french" => return word_by_word(&stem::french_light),
                "portuguese_light" | "light_portuguese" => {
                    return word_by_word(&stem::portuguese_light);
                }
                "italian_light" | "light_italian" => return word_by_word(&stem::italian_light),
                "spanish_light" | "light_spanish" => return word_by_word(&stem::spanish_light),
                "greek" => return word_by_word(&stem::greek),
                "galician" => return word_by_word(&rslp::galician),
                // the algorithms Snowball defines that BoostCore does not
                // carry, generated from the definitions themselves
                other @ ("catalan" | "basque" | "irish" | "lithuanian" | "estonian"
                | "armenian" | "porter" | "finnish") => {
                    let language = other.to_string();
                    return tokens
                        .into_iter()
                        .map(|(t, p, a, b, l)| {
                            let stemmed =
                                snowball::stem(&language, &t).unwrap_or_else(|| t.clone());
                            (stemmed, p, a, b, l)
                        })
                        .collect();
                }
                "german_light" | "light_german" => return word_by_word(&stem::german_light),
                "persian" => return word_by_word(&stem::persian),
                "german" => return word_by_word(&stem::german),
                _ => {}
            }
            // a stemmer written for a script has nothing to say about a word
            // written in another one
            let script_of = |lang: &str| -> Option<fn(char) -> bool> {
                match lang {
                    "arabic" | "persian" => Some(|c: char| ('\u{0600}'..='\u{06FF}').contains(&c)),
                    "greek" => Some(|c: char| ('\u{0370}'..='\u{03FF}').contains(&c)),
                    "russian" | "bulgarian" | "serbian" => {
                        Some(|c: char| ('\u{0400}'..='\u{04FF}').contains(&c))
                    }
                    _ => None,
                }
            };
            let its_own = script_of(&lang.to_ascii_lowercase());
            // the eighteen languages BoostCore carries an algorithm for
            if let Some(l) = language(lang) {
                let mut analyzer =
                    TextAnalyzer::builder(RawTokenizer::default()).filter(Stemmer::new(l)).build();
                return tokens
                    .into_iter()
                    .map(|(t, p, a, b, l)| {
                        if let Some(written_in) = its_own
                            && !t.chars().any(written_in)
                        {
                            return (t, p, a, b, l);
                        }
                        let stemmed = {
                            let mut s = analyzer.token_stream(&t);
                            if s.advance() { s.token().text.clone() } else { t.clone() }
                        };
                        (stemmed, p, a, b, l)
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
            .filter(|(t, _, _, _, _)| t.chars().count() >= *min && t.chars().count() <= *max)
            .collect(),
        Step::Trim => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| (t.trim().to_string(), p, a, b, l))
            .filter(|(t, _, _, _, _)| !t.is_empty())
            .collect(),
        Step::Reverse => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| (t.chars().rev().collect(), p, a, b, l))
            .collect(),
        Step::Unique => {
            let mut seen = std::collections::HashSet::new();
            tokens.into_iter().filter(|(t, _, _, _, _)| seen.insert(t.clone())).collect()
        }
        Step::UniqueAtPosition => {
            let mut seen = std::collections::HashSet::new();
            tokens.into_iter().filter(|(t, p, _, _, _)| seen.insert((*p, t.clone()))).collect()
        }
        // the marker itself does nothing: what it means is written into the
        // steps that follow it when the chain is built
        Step::KeywordRepeat => tokens,
        Step::Truncate(n) => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| (t.chars().take(*n).collect(), p, a, b, l))
            .collect(),
        Step::Limit(n) => tokens.into_iter().take(*n).collect(),
        Step::Synonym(rules) => {
            let mut out: Vec<Token> = Vec::new();
            let mut i = 0;
            // a graph lays the ways of reading a span out side by side, and
            // what comes after the span stands that much further along
            let mut shift = 0usize;
            while i < tokens.len() {
                // the longest rule that fits here is the one that stands
                let matched = rules
                    .iter()
                    .filter(|rule| {
                        !rule.phrase.is_empty()
                            && i + rule.phrase.len() <= tokens.len()
                            && rule
                                .phrase
                                .iter()
                                .enumerate()
                                .all(|(at, word)| tokens[i + at].0.to_lowercase() == *word)
                    })
                    .max_by_key(|rule| rule.phrase.len());
                let Some(rule) = matched else {
                    let (t, p, a, b, l) = tokens[i].clone();
                    out.push((t, p + shift, a, b, l));
                    i += 1;
                    continue;
                };
                let (_, position, from, _, _) = tokens[i].clone();
                let position = position + shift;
                let to = tokens[i + rule.phrase.len() - 1].3;
                if rule.graph {
                    // every way of reading the span is a path: the first word
                    // of each stands where the span starts, the rest take the
                    // next places nobody has taken, and the last word of each
                    // reaches to where the span ends
                    let mut paths: Vec<Vec<(String, usize, usize)>> = Vec::new();
                    for alternative in &rule.alternatives {
                        paths.push(alternative.iter().map(|w| (w.clone(), from, to)).collect());
                    }
                    if rule.keep_original || rule.alternatives.is_empty() {
                        paths.push(
                            (0..rule.phrase.len())
                                .map(|at| {
                                    let (text, _, a, b, _) = tokens[i + at].clone();
                                    (text, a, b)
                                })
                                .collect(),
                        );
                    }
                    let span = 1 + paths.iter().map(|p| p.len() - 1).sum::<usize>();
                    let mut next = position + 1;
                    let mut laid: Vec<Token> = Vec::new();
                    for path in &paths {
                        let mut places: Vec<usize> = vec![position];
                        for _ in 1..path.len() {
                            places.push(next);
                            next += 1;
                        }
                        for (k, (text, a, b)) in path.iter().enumerate() {
                            let here = places[k];
                            let reach = match places.get(k + 1) {
                                Some(after) => after - here,
                                None => position + span - here,
                            };
                            laid.push((text.clone(), here, *a, *b, reach));
                        }
                    }
                    laid.sort_by_key(|t| t.1);
                    out.extend(laid);
                    shift += span - rule.phrase.len();
                    i += rule.phrase.len();
                    continue;
                }
                let mut written = Vec::new();
                if rule.keep_original || rule.alternatives.is_empty() {
                    for at in 0..rule.phrase.len() {
                        let (text, _, from, to, _) = tokens[i + at].clone();
                        written.push((text, position + at, from, to, 1));
                    }
                }
                let mut meant = Vec::new();
                for alternative in &rule.alternatives {
                    // stacked in place, the last word of what is meant reaches
                    // to where the words it stands for end
                    let reach = (rule.phrase.len() + 1).saturating_sub(alternative.len()).max(1);
                    for (at, word) in alternative.iter().enumerate() {
                        let length = if at + 1 == alternative.len() { reach } else { 1 };
                        meant.push((word.clone(), position + at, from, to, length));
                    }
                }
                // stacked in place, the words are read in the order of the
                // places they stand in, the written word before what it means
                let start = out.len();
                if rule.alternatives_first {
                    out.extend(meant);
                    out.extend(written);
                } else {
                    out.extend(written);
                    out.extend(meant);
                }
                out[start..].sort_by_key(|t| t.1);
                i += rule.phrase.len();
            }
            out
        }
        Step::Fingerprint(separator) => {
            let mut words: Vec<String> = tokens.iter().map(|(t, _, _, _, _)| t.clone()).collect();
            words.sort();
            words.dedup();
            if words.is_empty() {
                return Vec::new();
            }
            let end = tokens.last().map(|(_, _, _, b, _)| *b).unwrap_or(0);
            vec![(words.join(&separator.to_string()), 0, 0, end, 1)]
        }
        Step::Elision => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| {
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
                (cut, p, a, b, l)
            })
            .filter(|(t, _, _, _, _)| !t.is_empty())
            .collect(),
        Step::IrishLowercase => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| {
                let chars: Vec<char> = t.chars().collect();
                // a prefix of one letter, and the word it stands in front of
                let written: String = match chars.first() {
                    Some('n') | Some('t') if chars.len() > 1 && is_irish_vowel(chars[1]) => {
                        chars.into_iter().collect()
                    }
                    _ => t.to_lowercase(),
                };
                (written.to_lowercase(), p, a, b, l)
            })
            .collect(),
        Step::GreekLowercase => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| (stem::greek_lowercase(&t), p, a, b, l))
            .collect(),
        Step::PersianNormalize => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| (stem::persian_normalize(&t), p, a, b, l))
            .filter(|(t, _, _, _, _)| !t.is_empty())
            .collect(),
        Step::RomanianNormalize => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| {
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
                (written, p, a, b, l)
            })
            .collect(),
        Step::DecimalDigits => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| (stem::decimal_digits(&t), p, a, b, l))
            .collect(),
        // handled before the match, where the held-back words are recorded
        Step::KeywordMarker(_) => tokens,
        Step::BaseForm(language) => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| {
                let base = morph::words(*language, &t)
                    .into_iter()
                    .next()
                    .and_then(|w| w.base)
                    .unwrap_or_else(|| t.clone());
                (base, p, a, b, l)
            })
            .collect(),
        Step::PartOfSpeech { language, stoptags } => tokens
            .into_iter()
            .filter(|(t, _, a, b, _)| {
                // what the dictionary called it where it stood, and only
                // failing that what it would be called on its own
                let part =
                    held.parts.get(&(*a, *b)).cloned().or_else(|| {
                        morph::words(*language, t).into_iter().next().and_then(|w| w.part)
                    });
                match (&part, stoptags) {
                    // a filter that names its own tags drops those and no others
                    (Some(part), Some(tags)) => !tags.iter().any(|tag| part.starts_with(tag)),
                    (Some(part), None) => !morph::is_grammar(part),
                    (None, _) => true,
                }
            })
            .collect(),
        Step::Reading(language) => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| {
                let reading = morph::words(*language, &t)
                    .into_iter()
                    .next()
                    .and_then(|w| w.reading)
                    .unwrap_or_else(|| t.clone());
                (reading, p, a, b, l)
            })
            .collect(),
        Step::KatakanaStem { minimum } => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| {
                let long = t.chars().count() >= *minimum;
                let all_katakana = t.chars().all(|c| matches!(c, 'ァ'..='ヶ' | 'ー'));
                match long && all_katakana && t.ends_with('ー') {
                    true => (t.trim_end_matches('ー').to_string(), p, a, b, l),
                    false => (t, p, a, b, l),
                }
            })
            .collect(),
        Step::Completion { index } => {
            // the word stays where it is and its readings stand beside it, so
            // a search for `sushi` and one for `寿司` find the same document
            let mut out = Vec::new();
            for (t, p, a, b, l) in tokens {
                let reading = morph::words(morph::Language::Japanese, &t)
                    .into_iter()
                    .next()
                    .and_then(|w| w.reading);
                // a word already written in kana is its own reading
                let reading = reading.unwrap_or_else(|| t.clone());
                let kunrei = romaji::of(&reading, romaji::System::Kunrei);
                let hepburn = romaji::of(&reading, romaji::System::Hepburn);
                // at search time the word is what was typed, not what it
                // stands for, so only the readings are offered
                if !index {
                    out.push((t, p, a, b, l));
                    continue;
                }
                out.push((t.clone(), p, a, b, l));
                for written in [kunrei, hepburn] {
                    if written != t && !out.iter().any(|(o, op, _, _, _)| *o == written && *op == p)
                    {
                        out.push((written, p, a, b, 1));
                    }
                }
            }
            out
        }
        Step::MinHash { buckets, hashes } => {
            let mut out = Vec::with_capacity(*buckets);
            for bucket in 0..*buckets {
                let mut smallest = u64::MAX;
                for (t, _, _, _, _) in &tokens {
                    for hash in 0..(*hashes).max(1) {
                        let mut seed = (bucket as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
                            ^ (hash as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                        for byte in t.as_bytes() {
                            seed = (seed ^ *byte as u64).wrapping_mul(0x1000_0000_01B3);
                        }
                        smallest = smallest.min(seed);
                    }
                }
                if smallest == u64::MAX {
                    smallest = 0;
                }
                out.push((format!("{smallest:016x}"), bucket, 0, 0, 1));
            }
            out
        }
        Step::IcuNormalize(set) => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| {
                let written = match set {
                    Some(set) => unicode_set::within(&t, set, |c| icu_normalize(c)),
                    None => icu_normalize(&t),
                };
                (written, p, a, b, l)
            })
            .collect(),
        Step::KoreanNumber => {
            // the dictionary reads `십만이천오백` as four words and it is one
            // number, so a run of them is joined before it is read -- and a
            // run that turns out not to be a number is put back as it was
            let mut out: Vec<Token> = Vec::new();
            let mut run: Vec<Token> = Vec::new();
            let close = |run: &mut Vec<Token>, out: &mut Vec<Token>| {
                while run.last().map(|(t, _, _, _, _)| korean_number::is_point(t)).unwrap_or(false)
                {
                    // a point with no digits after it is not part of a number
                    let last = run.pop().expect("just looked");
                    out.push(last);
                }
                if run.is_empty() {
                    return;
                }
                let joined: String = run.iter().map(|(t, _, _, _, _)| t.as_str()).collect();
                let (_, position, from, _, _) = run[0].clone();
                let to = run.last().expect("not empty").3;
                match korean_number::of(&joined) {
                    Some(digits) => out.push((digits, position, from, to, 1)),
                    None => out.append(run),
                }
                run.clear();
            };
            for token in tokens {
                let text = token.0.as_str();
                let continues = korean_number::is_numeral(text)
                    || (korean_number::is_point(text) && !run.is_empty());
                if continues {
                    run.push(token);
                    continue;
                }
                close(&mut run, &mut out);
                out.push(token);
            }
            close(&mut run, &mut out);
            // the positions are the ones the shorter list has
            out.into_iter().enumerate().map(|(at, (t, _, a, b, l))| (t, at, a, b, l)).collect()
        }
        Step::Collate { strength } => {
            tokens.into_iter().map(|(t, p, a, b, l)| (collate(&t, *strength), p, a, b, l)).collect()
        }
        Step::IcuFold(set) => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| {
                let written = match set {
                    Some(set) => unicode_set::within(&t, set, |c| icu_fold(c)),
                    None => icu_fold(&t),
                };
                (written, p, a, b, l)
            })
            .collect(),
        Step::Uppercase => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| {
                // a letter with no upper case of its own is left as it is,
                // which is what a filter written in Java does with `ß`
                let written: String = t
                    .chars()
                    .map(|c| {
                        let mut upper = c.to_uppercase();
                        match (upper.len(), upper.next()) {
                            (1, Some(one)) => one,
                            _ => c,
                        }
                    })
                    .collect();
                (written, p, a, b, l)
            })
            .collect(),
        Step::KStem => {
            tokens.into_iter().map(|(t, p, a, b, l)| (kstem::stem(&t), p, a, b, l)).collect()
        }
        Step::Phonetic { encoder, replace, languages, max_code_len, name_type, rule_type } => {
            let how = phonetic::How {
                encoder,
                languages,
                max_code_len: *max_code_len,
                name_type,
                rule_type,
            };
            let mut out = Vec::new();
            for (t, p, a, b, l) in tokens {
                match phonetic::encode(&how, &t) {
                    // a code and the word it was made from stand in the same
                    // place: a search for either finds the document
                    Some(code) if *replace => out.push((code, p, a, b, l)),
                    Some(code) => {
                        out.push((code, p, a, b, l.clone()));
                        out.push((t, p, a, b, l));
                    }
                    // an encoder that has nothing to say about a word leaves
                    // the word alone
                    None => out.push((t, p, a, b, l)),
                }
            }
            out
        }
        Step::Apostrophe => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| {
                let cut = t.split_once('\'').map(|(head, _)| head.to_string()).unwrap_or(t);
                (cut, p, a, b, l)
            })
            .collect(),
        Step::Possessive => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| {
                let base = t
                    .strip_suffix("'s")
                    .or_else(|| t.strip_suffix("'S"))
                    .or_else(|| t.strip_suffix("\u{2019}s"))
                    .map(|w| w.to_string())
                    .unwrap_or(t);
                (base, p, a, b, l)
            })
            .collect(),
        Step::Classic => {
            tokens.into_iter().map(|(t, p, a, b, l)| (stem::classic(&t), p, a, b, l)).collect()
        }
        Step::CjkWidth => {
            tokens.into_iter().map(|(t, p, a, b, l)| (widened(&t), p, a, b, l)).collect()
        }
        Step::Normalize(script) => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| (stem::normalize(script, &t), p, a, b, l))
            .filter(|(t, _, _, _, _)| !t.is_empty())
            .collect(),
        Step::PatternCapture { patterns, preserve_original } => {
            let mut out = Vec::new();
            for (t, p, a, b, l) in tokens {
                if *preserve_original {
                    out.push((t.clone(), p, a, b, l));
                }
                for pattern in patterns {
                    let Ok(re) = regex::Regex::new(pattern) else { continue };
                    for caps in re.captures_iter(&t) {
                        // the groups the pattern names, or the whole match
                        // when it names none
                        let named: Vec<&str> =
                            caps.iter().skip(1).flatten().map(|m| m.as_str()).collect();
                        if named.is_empty() {
                            if let Some(all) = caps.get(0) {
                                out.push((all.as_str().to_string(), p, a, b, l));
                            }
                        } else {
                            for group in named {
                                out.push((group.to_string(), p, a, b, l));
                            }
                        }
                    }
                }
            }
            out
        }
        Step::PatternReplace { pattern, replacement } => {
            let Ok(re) = regex::Regex::new(pattern) else { return tokens };
            tokens
                .into_iter()
                .map(|(t, p, a, b, l)| {
                    (re.replace_all(&t, replacement.as_str()).into_owned(), p, a, b, l)
                })
                .filter(|(t, _, _, _, _)| !t.is_empty())
                .collect()
        }
        Step::Keep { words, keep } => {
            let set: std::collections::HashSet<String> =
                words.iter().map(|w| w.to_lowercase()).collect();
            tokens
                .into_iter()
                .filter(|(t, _, _, _, _)| set.contains(&t.to_lowercase()) == *keep)
                .collect()
        }
        Step::KeepTypes { types, keep } => tokens
            .into_iter()
            .filter(|(t, _, _, _, _)| {
                let kind = if t.chars().all(|c| c.is_numeric()) { "<NUM>" } else { "<ALPHANUM>" };
                types.iter().any(|named| named == kind) == *keep
            })
            .collect(),
        Step::WordDelimiter { catenate, on_numerics, on_case_change, adjust_offsets } => {
            let mut out: Vec<Token> = Vec::new();
            for (t, p, a, b, l) in tokens {
                let parts = split_where_writing_changes(&t, *on_numerics, *on_case_change);
                if parts.len() < 2 {
                    out.push((t, p, a, b, l));
                    continue;
                }
                // each part stands where it was written, unless the request
                // asked for the offsets of the word it came from
                let mut at = a;
                for part in &parts {
                    let (from, to) = if *adjust_offsets { (at, at + part.len()) } else { (a, b) };
                    out.push((part.clone(), p, from, to, 1));
                    at += part.len();
                }
                if *catenate {
                    out.push((parts.concat(), p, a, b, 1));
                }
            }
            // splitting a word makes new positions, and what follows it stands
            // further along than it did
            for (at, token) in out.iter_mut().enumerate() {
                token.1 = at;
            }
            out
        }
        Step::CommonGrams(words) => {
            let common: std::collections::HashSet<String> =
                words.iter().map(|w| w.to_lowercase()).collect();
            let mut out = Vec::new();
            for (i, (t, p, a, b, l)) in tokens.iter().enumerate() {
                out.push((t.clone(), *p, *a, *b, *l));
                if let Some((next, _, _, nb, _)) = tokens.get(i + 1)
                    && (common.contains(&t.to_lowercase()) || common.contains(&next.to_lowercase()))
                {
                    out.push((format!("{t}_{next}"), *p, *a, *nb, 2));
                }
            }
            out
        }
        Step::Shingle { min, max, unigrams, unigrams_if_none, separator, filler } => {
            let mut out: Vec<Token> = Vec::new();
            let mut shingled = false;
            for (i, (t, p, a, b, l)) in tokens.iter().enumerate() {
                if *unigrams {
                    out.push((t.clone(), *p, *a, *b, *l));
                }
                for width in *min..=*max {
                    if width < 2 {
                        continue;
                    }
                    // a run that runs off the end is written with the filler
                    // standing in for the words that are not there
                    let mut words: Vec<String> = Vec::new();
                    let mut end = *b;
                    for step in 0..width {
                        match tokens.get(i + step) {
                            Some((word, _, _, to, _)) => {
                                words.push(word.clone());
                                end = *to;
                            }
                            None => words.push(filler.clone()),
                        }
                    }
                    if i + width > tokens.len() {
                        continue;
                    }
                    out.push((words.join(separator), *p, *a, end, width));
                    shingled = true;
                }
            }
            match !shingled && !*unigrams && *unigrams_if_none {
                true => tokens,
                false => out,
            }
        }
        Step::DelimitedTermFreq(sep) => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| {
                let word = t.split(*sep).next().unwrap_or(&t).to_string();
                (word, p, a, b, l)
            })
            .collect(),
        Step::DelimitedPayload(sep) => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| {
                let word = t.split(*sep).next().unwrap_or(&t).to_string();
                (word, p, a, b, l)
            })
            .collect(),
        // the payload is read back by a term vector, not by the stream
        Step::TypeAsPayload => tokens,
        Step::FlattenGraph => {
            // each node of the graph is placed one after the furthest of the
            // nodes that reach it, and every token is as long as the gap
            // between the nodes it joins
            let mut nodes: Vec<usize> =
                tokens.iter().flat_map(|(_, p, _, _, l)| [*p, *p + *l]).collect();
            nodes.sort_unstable();
            nodes.dedup();
            let mut placed: std::collections::HashMap<usize, usize> = Default::default();
            for node in &nodes {
                let reached = tokens
                    .iter()
                    .filter(|(_, p, _, _, l)| p + l == *node)
                    .filter_map(|(_, p, _, _, _)| placed.get(p).map(|at| at + 1))
                    .max();
                let at =
                    reached.unwrap_or(placed.values().copied().max().map(|m| m + 1).unwrap_or(0));
                placed.insert(*node, at);
            }
            let mut out: Vec<Token> = tokens
                .into_iter()
                .map(|(t, p, a, b, l)| {
                    let from = placed.get(&p).copied().unwrap_or(p);
                    let to = placed.get(&(p + l)).copied().unwrap_or(from + 1);
                    (t, from, a, b, to.saturating_sub(from).max(1))
                })
                .collect();
            out.sort_by_key(|t| t.1);
            out
        }
        Step::Multiplexer(branches) => {
            // every branch is applied to the whole stream, and the results
            // are read back token by token: each word, in each of its forms,
            // before the next word
            let mut out: Vec<Token> = Vec::new();
            for branch in branches {
                let mut here = tokens.clone();
                for step in branch {
                    here = apply_step(step, here, held);
                }
                out.extend(here);
            }
            out.sort_by_key(|t| t.1);
            out
        }
        Step::StemmerOverride(map) => tokens
            .into_iter()
            .map(|(t, p, a, b, l)| match map.get(&t.to_lowercase()) {
                Some(told) => (told.clone(), p, a, b, l),
                None => (t, p, a, b, l),
            })
            .collect(),
        Step::Decompound(parts) => {
            let mut out = Vec::new();
            for (t, p, a, b, l) in tokens {
                out.push((t.clone(), p, a, b, l));
                for part in parts {
                    if t.len() > part.len() && t.to_lowercase().contains(&part.to_lowercase()) {
                        out.push((part.clone(), p, a, b, 1));
                    }
                }
            }
            out
        }
        Step::NgramTokens { min, max, edges } => {
            let mut out = Vec::new();
            for (t, p, a, b, _) in tokens {
                let chars: Vec<char> = t.chars().collect();
                for start in 0..chars.len() {
                    if *edges && start > 0 {
                        break;
                    }
                    for size in *min..=*max {
                        if start + size <= chars.len() {
                            out.push((
                                chars[start..start + size].iter().collect::<String>(),
                                p,
                                a,
                                b,
                                1,
                            ));
                        }
                    }
                }
            }
            out
        }
        Step::PreserveOriginal(inner) => {
            let mut out = apply_step(inner, tokens.clone(), held);
            out.extend(tokens);
            out
        }
        Step::CjkBigram => {
            let mut out = Vec::new();
            for (t, p, a, b, l) in tokens {
                let chars: Vec<char> = t.chars().collect();
                // a word written in an alphabet is left as it is
                if chars.len() < 2 || !chars.iter().any(|c| is_cjk(*c)) {
                    out.push((t, p, a, b, l));
                    continue;
                }
                for (i, pair) in chars.windows(2).enumerate() {
                    out.push((pair.iter().collect::<String>(), p + i, a + i, a + i + 2, 1));
                }
            }
            out
        }
    }
}

/// A word written the one way Unicode says it is written, in the case it is
/// compared in.
///
/// This is what `icu_normalizer` does: NFKC, and then the case a comparison
/// uses -- which writes the German sharp s as two letters, the way a reader
/// typing it on a keyboard without one would.
/// How much of a difference between two words counts as a difference.
///
/// A collation compares in passes: the letters first, then the marks written
/// on them, then the case, then the punctuation. A strength says which pass
/// to stop after, and everything past it is a difference the comparison does
/// not see.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strength {
    /// the letters only: `bâton` and `Baton` are the same word
    Primary,
    /// the marks count: `bâton` and `baton` are different, `Baton` is not
    Secondary,
    /// the case counts too
    Tertiary,
}

/// A word as the words it is equal to at this strength.
///
/// This is a folding, not a sort key. A collation proper answers "which of
/// these two comes first in this language", which is a question about a
/// locale's own order -- Swedish puts `ä` after `z`, and no folding of the
/// letters can say that. What this answers is the question the filter is
/// used for: whether two words are the same at a given strength, so that a
/// search for one finds the other. Sorting on a field this produced would
/// sort by the folded text, which is the right order for most of the Latin
/// alphabet and not a claim about any particular language's.
fn collate(word: &str, strength: Strength) -> String {
    match strength {
        // the marks and the case both dropped
        Strength::Primary => icu_fold(word),
        // the marks kept, the case dropped
        Strength::Secondary => icu_normalize(word),
        // everything kept but the way the characters are written
        Strength::Tertiary => {
            use icu_normalizer::ComposingNormalizerBorrowed;
            ComposingNormalizerBorrowed::new_nfkc().normalize(word).into_owned()
        }
    }
}

/// The parts of speech a filter was told to drop, where it was told any.
fn stoptags_of(spec: &Value) -> Option<Vec<String>> {
    let listed = spec.get("stoptags")?.as_array()?;
    Some(listed.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
}

/// What a chain carries from one filter to the next.
struct Held {
    /// words a `keyword_marker` protected from the stemmers
    protected: std::collections::HashSet<String>,
    /// what the dictionary called each word, by the span it stood in
    parts: std::collections::HashMap<(usize, usize), String>,
}

impl Held {
    /// Nothing carried: a chain that has no dictionary behind it and has
    /// protected nothing yet.
    fn new() -> Held {
        Held { protected: Default::default(), parts: Default::default() }
    }
}

/// The set a filter's settings name, if it names one this can read.
fn unicode_set_of(spec: &Value) -> Option<unicode_set::UnicodeSet> {
    spec.get("unicode_set_filter").and_then(|v| v.as_str()).and_then(unicode_set::UnicodeSet::parse)
}

pub(crate) fn icu_normalize(word: &str) -> String {
    use icu_normalizer::ComposingNormalizerBorrowed;
    let folded: String = word
        .chars()
        .flat_map(|c| c.to_lowercase())
        .collect::<String>()
        .replace(['\u{00DF}', '\u{1E9E}'], "ss");
    ComposingNormalizerBorrowed::new_nfkc().normalize(&folded).into_owned()
}

/// The same, with the marks written on the letters dropped: what
/// `icu_folding` does.
pub(crate) fn icu_fold(word: &str) -> String {
    use icu_normalizer::DecomposingNormalizerBorrowed;
    let normalized = icu_normalize(word);
    let decomposed = DecomposingNormalizerBorrowed::new_nfkd().normalize(&normalized);
    decomposed.chars().filter(|c| !('\u{0300}'..='\u{036F}').contains(c)).collect()
}

/// Where one word ends and the next begins.
///
/// Unicode says where a break may fall, and for Thai, Lao, Khmer, Burmese,
/// Chinese and Japanese -- written without spaces between words -- a
/// dictionary says which of those breaks are real ones.
/// Words the way Java's break iterator cuts them for the Thai analyzer:
/// Thai runs by dictionary, and Latin runs whole, a hyphen, an apostrophe
/// or an underscore between letters being part of the word.
fn thai_words(text: &str) -> Vec<Token> {
    let pieces = icu_words(text);
    let bytes = text.as_bytes();
    let mut out: Vec<Token> = Vec::new();
    for (t, _, a, b, l) in pieces {
        // joined onto the word before it where only a joiner lies between
        if let Some(last) = out.last_mut()
            && b > a
            && last.3 < a
            && text[last.3..a].chars().all(|c| matches!(c, '-' | '\'' | '\u{2019}'))
            && text[last.3..a].chars().count() == 1
            && bytes[last.3 - 1].is_ascii_alphanumeric()
            && bytes[a].is_ascii_alphanumeric()
        {
            last.0 = text[last.2..b].to_string();
            last.3 = b;
            continue;
        }
        out.push((t, out.len(), a, b, l));
    }
    out
}

fn icu_words(text: &str) -> Vec<Token> {
    use icu_segmenter::WordSegmenter;
    use icu_segmenter::options::WordBreakInvariantOptions;
    let segmenter = WordSegmenter::new_auto(WordBreakInvariantOptions::default());
    let mut out = Vec::new();
    let mut last = 0usize;
    for (at, kind) in segmenter.segment_str(text).iter_with_word_type() {
        if at > last && kind.is_word_like() {
            out.push((text[last..at].to_string(), out.len(), last, at, 1));
        }
        last = at;
    }
    out
}

/// The letters written narrow, written the width the index keeps them at.
///
/// The fullwidth Latin letters are the plain ones, and the halfwidth katakana
/// are the ordinary katakana -- with the mark for a voiced sound joined back
/// onto the letter it belongs to.
fn widened(text: &str) -> String {
    const HALFWIDTH_KATAKANA: &str = concat!(
        "\u{3002}\u{300C}\u{300D}\u{3001}\u{30FB}\u{30F2}\u{30A1}\u{30A3}\u{30A5}\u{30A7}",
        "\u{30A9}\u{30E3}\u{30E5}\u{30E7}\u{30C3}\u{30FC}\u{30A2}\u{30A4}\u{30A6}\u{30A8}",
        "\u{30AA}\u{30AB}\u{30AD}\u{30AF}\u{30B1}\u{30B3}\u{30B5}\u{30B7}\u{30B9}\u{30BB}",
        "\u{30BD}\u{30BF}\u{30C1}\u{30C4}\u{30C6}\u{30C8}\u{30CA}\u{30CB}\u{30CC}\u{30CD}",
        "\u{30CE}\u{30CF}\u{30D2}\u{30D5}\u{30D8}\u{30DB}\u{30DE}\u{30DF}\u{30E0}\u{30E1}",
        "\u{30E2}\u{30E4}\u{30E6}\u{30E8}\u{30E9}\u{30EA}\u{30EB}\u{30EC}\u{30ED}\u{30EF}",
        "\u{30F3}\u{309B}\u{309C}"
    );
    let wide: Vec<char> = HALFWIDTH_KATAKANA.chars().collect();
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let n = c as u32;
        if (0xFF01..=0xFF5E).contains(&n) {
            out.push(char::from_u32(n - 0xFEE0).unwrap_or(c));
        } else if n == 0x3000 {
            out.push(' ');
        } else if (0xFF61..=0xFF9F).contains(&n) {
            match wide.get((n - 0xFF61) as usize) {
                Some(w) => out.push(*w),
                None => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Where a word changes from letters to digits, or from lower case to upper.
fn split_where_writing_changes(word: &str, on_numerics: bool, on_case: bool) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut last: Option<char> = None;
    for c in word.chars() {
        if !c.is_alphanumeric() {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            last = None;
            continue;
        }
        if let Some(previous) = last {
            let changed = (on_numerics && previous.is_numeric() != c.is_numeric())
                || (on_case && previous.is_lowercase() && c.is_uppercase());
            if changed && !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
        }
        current.push(c);
        last = Some(c);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// The vowels Irish writes, with and without the mark on them.
fn is_irish_vowel(c: char) -> bool {
    matches!(
        c,
        'a' | 'e'
            | 'i'
            | 'o'
            | 'u'
            | 'A'
            | 'E'
            | 'I'
            | 'O'
            | 'U'
            | '\u{00E1}'
            | '\u{00E9}'
            | '\u{00ED}'
            | '\u{00F3}'
            | '\u{00FA}'
    )
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
        "_danish_" | "danish" => &[
            "og",
            "i",
            "jeg",
            "det",
            "at",
            "en",
            "den",
            "til",
            "er",
            "som",
            "p\u{00E5}",
            "de",
            "med",
            "han",
            "af",
            "for",
            "ikke",
            "der",
            "var",
            "mig",
            "sig",
            "men",
            "et",
            "har",
            "om",
            "vi",
            "min",
            "havde",
            "ham",
            "hun",
            "nu",
            "over",
            "da",
            "fra",
            "du",
            "ud",
            "sin",
            "dem",
            "os",
            "op",
            "man",
            "hans",
            "hvor",
            "eller",
            "hvad",
            "skal",
            "selv",
            "her",
            "alle",
            "vil",
            "blev",
            "kunne",
            "ind",
            "n\u{00E5}r",
            "v\u{00E6}re",
            "dog",
            "noget",
            "ville",
            "jo",
            "deres",
            "efter",
            "ned",
            "skulle",
            "denne",
            "end",
            "dette",
            "mit",
            "og\u{00E5}",
            "under",
            "have",
            "dig",
            "anden",
            "hende",
            "mine",
            "alt",
            "meget",
            "sit",
            "sine",
            "vor",
            "mod",
            "disse",
            "hvis",
            "din",
            "nogle",
            "hos",
            "blive",
            "mange",
            "ad",
            "bliver",
            "hendes",
            "v\u{00E6}ret",
            "thi",
            "jer",
            "s\u{00E5}dan",
        ],
        "_norwegian_" | "norwegian" => &[
            "og",
            "i",
            "jeg",
            "det",
            "at",
            "en",
            "et",
            "den",
            "til",
            "er",
            "som",
            "p\u{00E5}",
            "de",
            "med",
            "han",
            "av",
            "ikke",
            "der",
            "s\u{00E5}",
            "var",
            "meg",
            "seg",
            "men",
            "ett",
            "har",
            "om",
            "vi",
            "min",
            "mitt",
            "ha",
            "hadde",
            "hun",
            "n\u{00E5}",
            "over",
            "da",
            "ved",
            "fra",
            "du",
            "ut",
            "sin",
            "dem",
            "oss",
            "opp",
            "man",
            "kan",
            "hans",
            "hvor",
            "eller",
            "hva",
            "skal",
            "selv",
            "sj\u{00F8}l",
            "her",
            "alle",
            "vil",
            "bli",
            "ble",
            "blitt",
            "kunne",
            "inn",
            "n\u{00E5}r",
            "v\u{00E6}re",
            "kom",
            "noen",
            "noe",
            "ville",
            "dere",
            "som",
            "deres",
            "kun",
            "ja",
            "etter",
            "ned",
            "skulle",
            "denne",
            "for",
            "deg",
            "si",
            "sine",
            "sitt",
            "mot",
            "\u{00E5}",
            "meget",
            "hvorfor",
            "dette",
            "disse",
            "uten",
            "hvordan",
            "ingen",
            "din",
            "ditt",
            "blir",
            "samme",
            "hvilken",
            "hvilke",
            "s\u{00E5}nn",
            "inni",
            "mellom",
            "v\u{00E5}r",
            "hver",
            "hvem",
            "vors",
            "hvis",
            "b\u{00E5}de",
            "bare",
            "enn",
            "fordi",
            "f\u{00F8}r",
            "mange",
            "ogs\u{00E5}",
            "slik",
            "v\u{00E6}rt",
            "v\u{00E6}re",
            "b\u{00E5}e",
            "begge",
            "siden",
            "dykk",
        ],
        "_dutch_" | "dutch" => &[
            "de", "en", "van", "ik", "te", "dat", "die", "in", "een", "hij", "het", "niet", "zijn",
            "is", "was", "op", "aan", "met", "als", "voor", "had", "er", "maar", "om", "hem",
            "dan", "zou", "of", "wat", "mijn", "men", "dit", "zo", "door", "over", "ze", "zich",
            "bij", "ook", "tot", "je", "mij", "uit", "der", "daar", "haar", "naar", "heb", "hoe",
            "heeft", "hebben", "deze", "u", "want", "nog", "zal", "me", "zij", "nu", "ge", "geen",
            "omdat", "iets", "worden", "toch", "al", "waren", "veel", "meer", "doen", "toen",
            "moet", "ben", "zonder", "kan", "hun", "dus", "alles", "onder", "ja", "eens", "hier",
            "wie", "werd", "altijd", "doch", "wordt", "wezen", "kunnen", "ons", "zelf", "tegen",
            "na", "reeds", "wil", "kon", "niets", "uw", "iemand", "geweest", "andere",
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
    char_filters: Value,
}

impl Registry {
    /// Read the `analysis` an index's settings define, on top of the built-ins.
    pub fn from_settings(settings: &Value) -> Registry {
        let mut registry = Registry::default();
        // a setting may be written as one dotted key rather than as nested
        // objects; both spell the same thing
        let settings = &unflattened(settings);
        let analysis = settings
            .pointer("/index/analysis")
            .or_else(|| settings.pointer("/analysis"))
            .cloned()
            .unwrap_or(Value::Null);
        let filters = analysis.get("filter").cloned().unwrap_or(Value::Null);
        let tokenizers = analysis.get("tokenizer").cloned().unwrap_or(Value::Null);
        let chars = analysis.get("char_filter").cloned().unwrap_or(Value::Null);
        registry.char_filters = chars.clone();
        // a normalizer is an analyzer that does not cut the text: whatever it
        // names is applied to the value whole
        if let Some(defined) = analysis.get("normalizer").and_then(|n| n.as_object()) {
            for (name, spec) in defined {
                let mut spelled = spec.clone();
                if let Some(o) = spelled.as_object_mut() {
                    o.insert("tokenizer".into(), Value::String("keyword".into()));
                    o.insert("type".into(), Value::String("custom".into()));
                }
                if let Some(chain) = build_with(&spelled, &tokenizers, &filters, &chars) {
                    registry.named.insert(name.clone(), chain);
                }
            }
        }
        let Some(defined) = analysis.get("analyzer").and_then(|a| a.as_object()) else {
            registry.tokenizers = tokenizers;
            registry.filters = filters;
            return registry;
        };
        for (name, spec) in defined {
            if let Some(chain) = build_with(spec, &tokenizers, &filters, &chars) {
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
        build_with(spec, &self.tokenizers, &self.filters, &self.char_filters)
    }

    /// The tokenizer of a request on its own, with no filter over it: what
    /// `_analyze` with `explain` reports before the filters are applied.
    pub fn tokenizer_only(&self, spec: &Value) -> Chain {
        let source = match spec {
            Value::String(name) => tokenizer_source(name, &self.tokenizers),
            other => source_of_spec(other),
        };
        Chain { pre: Vec::new(), source, steps: Vec::new(), annotated: false }
    }

    /// What is wrong with the analysis an index's settings describe, if
    /// anything: a filter that cannot be built is refused when the index is
    /// created rather than when the first document is written to it.
    pub fn complaint(settings: &Value) -> Option<String> {
        let analysis = settings
            .pointer("/index/analysis")
            .or_else(|| settings.pointer("/analysis"))
            .cloned()
            .unwrap_or(Value::Null);
        let filters = analysis.get("filter").cloned().unwrap_or(Value::Null);
        for (name, spec) in analysis.get("filter").and_then(|f| f.as_object())?.iter() {
            if filter_of_spec(spec, &filters).is_none() {
                let kind = spec.get("type").and_then(|t| t.as_str()).unwrap_or("");
                return Some(match kind {
                    "elision" => {
                        "elision filter requires [articles] or [articles_path] setting".to_string()
                    }
                    other => format!("Unknown filter type [{other}] for [{name}]"),
                });
            }
        }
        None
    }

    /// The char filters a request or a mapping names, ready to be applied.
    pub fn char_filters(&self, named: &[Value]) -> Vec<CharFilter> {
        named.iter().filter_map(|one| self.char_filter(one)).collect()
    }

    /// One char filter, named or described.
    fn char_filter(&self, spec: &Value) -> Option<CharFilter> {
        let spec = match spec {
            Value::String(name) => self.char_filters.get(name).cloned().unwrap_or_else(|| {
                // a name with nothing behind it is one of the built-ins
                json!({"type": name})
            }),
            other => other.clone(),
        };
        let kind = spec.get("type").and_then(|t| t.as_str())?;
        let text = |k: &str| spec.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        Some(match kind {
            "html_strip" => {
                CharFilter::HtmlStrip(spec.get("escaped_tags").map(word_list).unwrap_or_default())
            }
            "mapping" => CharFilter::Mapping(
                spec.get("mappings")
                    .map(word_list)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|rule| rule.split_once("=>"))
                    .map(|(from, to)| (unescape(from.trim()), unescape(to.trim())))
                    .collect(),
            ),
            "pattern_replace" => {
                CharFilter::Replace { pattern: text("pattern"), replacement: text("replacement") }
            }
            "icu_normalizer" => CharFilter::IcuNormalize(
                spec.get("unicode_set_filter")
                    .and_then(|v| v.as_str())
                    .and_then(unicode_set::UnicodeSet::parse),
            ),
            _ => return None,
        })
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

    /// Whether the index defined an analyzer under this name itself.
    pub fn knows_named(&self, name: &str) -> bool {
        self.named.contains_key(name)
    }

    /// The chain a name stands for: the index's own first, then the built-ins.
    pub fn get(&self, name: &str) -> Option<Chain> {
        // a chain asked for by its annotated name is the chain it wraps, told
        // that the markup comes off first
        if let Some(base) = annotated_base(name) {
            let mut chain = self.get(base)?;
            chain.annotated = true;
            return Some(chain);
        }
        self.named.get(name).cloned().or_else(|| builtin(name))
    }

    pub fn names(&self) -> Vec<String> {
        self.named.keys().cloned().collect()
    }
}

/// An analyzer the index defined, out of the parts it named.
/// The same, told about the char filters the index defined.
fn build_with(spec: &Value, tokenizers: &Value, filters: &Value, chars: &Value) -> Option<Chain> {
    // a phone analyzer is named by type and told which country to read an
    // unprefixed number as belonging to
    if let Some(kind) = spec.get("type").and_then(|t| t.as_str())
        && (kind == "phone" || kind == "phone-search")
    {
        return Some(Chain {
            pre: Vec::new(),
            source: Source::Phone {
                region: spec
                    .get("phone-region")
                    .and_then(|r| r.as_str())
                    .unwrap_or("ZZ")
                    .to_string(),
                ngrams: kind == "phone",
            },
            steps: Vec::new(),
            annotated: false,
        });
    }
    // `{"type": "english"}` names a built-in rather than describing a chain
    if let Some(kind) = spec.get("type").and_then(|t| t.as_str())
        && kind != "custom"
        && let Some(mut chain) = spec
            .get("language")
            .and_then(|l| l.as_str())
            .and_then(|l| builtin(&l.to_ascii_lowercase()))
            .or_else(|| builtin(kind))
    {
        if let Some(list) = spec.get("stopwords") {
            let words = word_list(list);
            chain.steps.retain(|s| !matches!(s, Step::Stop(_)));
            chain.steps.push(Step::Stop(words));
        }
        return Some(chain);
    }
    // a tokenizer is named, or described where it is used -- `_analyze` sends
    // the description itself rather than a name the settings gave it
    let source = match spec.get("tokenizer") {
        Some(Value::Object(_)) => source_of_spec(spec.get("tokenizer").unwrap()),
        Some(Value::String(named)) => tokenizer_source(named, tokenizers),
        _ => tokenizer_source("standard", tokenizers),
    };
    let mut steps = Vec::new();
    for step in
        spec.get("filter").into_iter().flat_map(|f| f.as_array().cloned().unwrap_or_default())
    {
        // a filter is named, or described where it is used
        let found = match &step {
            Value::String(name) => token_filter(name, filters),
            other => filter_of_spec(other, filters),
        };
        if let Some(s) = found {
            steps.extend(s);
        }
    }
    let steps = stacked(steps);
    let pre = spec.get("char_filter").and_then(|c| c.as_array()).cloned().unwrap_or_default();
    let mut chain = Chain { pre: char_filters_of(&pre, chars), source, steps, annotated: false };
    chain.cut_synonyms();
    Some(chain)
}

/// The steps that follow a `keyword_repeat`, each keeping the word it was
/// given beside what it made of it.
///
/// Lucene writes the repeat as a second copy of the token marked as a keyword,
/// which the stemmers then leave alone; the two forms end up stacked in one
/// place. Keeping the original beside the stem says the same thing, and says
/// it without a mark that every filter would have to carry.
fn stacked(steps: Vec<Step>) -> Vec<Step> {
    if !steps.iter().any(|s| matches!(s, Step::KeywordRepeat)) {
        return steps;
    }
    let mut out = Vec::with_capacity(steps.len());
    let mut repeating = false;
    for step in steps {
        match step {
            Step::KeywordRepeat => repeating = true,
            other if repeating && other.stems() => {
                out.push(Step::PreserveOriginal(Box::new(other)));
            }
            other => out.push(other),
        }
    }
    out
}

/// Settings with every dotted key opened out into the objects it names.
fn unflattened(settings: &Value) -> Value {
    let Some(map) = settings.as_object() else { return settings.clone() };
    let mut out = Value::Object(serde_json::Map::new());
    for (key, value) in map {
        let value = match value {
            Value::Object(_) => unflattened(value),
            other => other.clone(),
        };
        let mut node = &mut out;
        let parts: Vec<&str> = key.split('.').collect();
        for part in &parts[..parts.len() - 1] {
            let o = node.as_object_mut().unwrap();
            node =
                o.entry(part.to_string()).or_insert_with(|| Value::Object(serde_json::Map::new()));
            if !node.is_object() {
                *node = Value::Object(serde_json::Map::new());
            }
        }
        if let Some(o) = node.as_object_mut() {
            let last = parts[parts.len() - 1].to_string();
            match (o.get_mut(&last), value) {
                (Some(Value::Object(held)), Value::Object(more)) => held.extend(more),
                (_, value) => {
                    o.insert(last, value);
                }
            }
        }
    }
    out
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
            // Java's `\W` is the ASCII one, and that is what a mapping
            // written for OpenSearch expects
            let pattern = text("pattern").unwrap_or(r"[^a-zA-Z0-9_]+").to_string();
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
        // the tokenizers that ask a dictionary where the words are
        "icu_tokenizer" | "thai" => Source::Icu,
        // and the ones whose dictionary also says what each word is
        // kuromoji reads for a search box by default, which means a long
        // compound is offered whole and in pieces
        "kuromoji_tokenizer" | "kuromoji" => Source::Morph {
            language: morph::Language::Japanese,
            drop_grammar: false,
            base_form: false,
            search: true,
        },
        "nori_tokenizer" | "nori" => Source::Morph {
            language: morph::Language::Korean,
            drop_grammar: false,
            base_form: false,
            search: false,
        },
        // the tokenizer keeps the punctuation, as one mark standing for all
        // of them; the `smartcn` analyzer's stop words are what drop it
        "smartcn_tokenizer" | "smartcn" => Source::Morph {
            language: morph::Language::Chinese,
            drop_grammar: false,
            base_form: false,
            search: false,
        },
        "lowercase" => Source::LetterLower,
        "classic" => Source::Classic,
        "uax_url_email" => Source::UaxUrlEmail,
        "path_hierarchy" | "PathHierarchy" => {
            Source::PathHierarchy { delimiter: '/', replacement: '/' }
        }
        "pattern" => Source::PatternSplit(r"[^a-zA-Z0-9_]+".into()),
        "ngram" => Source::Ngram { min: 1, max: 2, edges: false },
        "edge_ngram" => Source::Ngram { min: 1, max: 2, edges: true },
        _ => Source::Standard,
    }
}

/// A token filter by name, defined by the index or built in.
fn token_filter(name: &str, defined: &Value) -> Option<Vec<Step>> {
    if let Some(spec) = defined.get(name) {
        return filter_of_spec(spec, defined);
    }
    filter_of_name(name)
}

/// A filter described rather than named.
fn filter_of_spec(spec: &Value, defined: &Value) -> Option<Vec<Step>> {
    let kind = spec.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let num =
        |k: &str, d: usize| spec.get(k).and_then(|v| v.as_u64()).map(|v| v as usize).unwrap_or(d);
    let text = |k: &str| spec.get(k).and_then(|v| v.as_str());
    let one = |k: &str, d: char| text(k).and_then(|s| s.chars().next()).unwrap_or(d);
    let flag = |k: &str| spec.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    let script_of = |s: &Value| -> String {
        match s {
            Value::String(text) => text.clone(),
            other => other.get("source").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        }
    };
    let steps = match kind {
        // a script decides, token by token, whether the filters inside apply
        "condition" => {
            let inner: Vec<Step> = spec
                .get("filter")
                .and_then(|f| f.as_array())
                .map(|list| {
                    list.iter()
                        .flat_map(|f| match f {
                            Value::String(name) => token_filter(name, defined).unwrap_or_default(),
                            other => filter_of_spec(other, defined).unwrap_or_default(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            vec![Step::Condition {
                script: spec.get("script").map(script_of).unwrap_or_default(),
                inner,
            }]
        }
        "predicate_token_filter" => {
            vec![Step::Predicate(spec.get("script").map(script_of).unwrap_or_default())]
        }
        "stop" => vec![Step::Stop(match spec.get("stopwords") {
            Some(list) => word_list(list),
            None => stop_words("_english_"),
        })],
        "stemmer" | "snowball" => vec![Step::Stem(
            text("language").or_else(|| text("name")).unwrap_or("english").to_string(),
        )],
        "length" => vec![Step::Length { min: num("min", 0), max: num("max", usize::MAX) }],
        "phonetic" => vec![Step::Phonetic {
            encoder: text("encoder").unwrap_or("double_metaphone").to_string(),
            // the word a code was made from is dropped unless the filter is
            // told to keep it, which is what OpenSearch does
            replace: spec
                .get("replace")
                .and_then(|v| v.as_bool().or_else(|| v.as_str().map(|s| s == "true")))
                .unwrap_or(true),
            languages: spec
                .get("languageset")
                .map(|v| match v {
                    Value::Array(a) => {
                        a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()
                    }
                    Value::String(one) => one.split(',').map(|s| s.trim().to_string()).collect(),
                    _ => Vec::new(),
                })
                .unwrap_or_default(),
            max_code_len: spec
                .get("max_code_len")
                .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
                .map(|n| n as usize),
            name_type: text("name_type").unwrap_or("generic").to_string(),
            rule_type: text("rule_type").unwrap_or("approx").to_string(),
        }],
        "truncate" => vec![Step::Truncate(num("length", 10))],
        "limit" => vec![Step::Limit(num("max_token_count", 1))],
        "synonym" | "synonym_graph" => {
            vec![Step::Synonym(synonyms(spec, kind == "synonym_graph"))]
        }
        "lowercase" => vec![Step::Lowercase],
        "uppercase" => vec![Step::Uppercase],
        "asciifolding" => {
            if flag("preserve_original") {
                vec![Step::PreserveOriginal(Box::new(Step::AsciiFolding))]
            } else {
                vec![Step::AsciiFolding]
            }
        }
        "trim" => vec![Step::Trim],
        "reverse" => vec![Step::Reverse],
        "unique" => match flag("only_on_same_position") {
            true => vec![Step::UniqueAtPosition],
            false => vec![Step::Unique],
        },
        "elision" => {
            if spec.get("articles").is_none() && spec.get("articles_path").is_none() {
                return None;
            }
            vec![Step::Elision]
        }
        "kstem" => vec![Step::KStem],
        "porter_stem" => vec![Step::Stem("porter".into())],
        "fingerprint" => vec![Step::Fingerprint(one("separator", ' '))],
        "apostrophe" => vec![Step::Apostrophe],
        "classic" => vec![Step::Classic],
        "decimal_digit" => vec![Step::DecimalDigits],
        "cjk_width" => vec![Step::CjkWidth],
        "cjk_bigram" => vec![Step::CjkBigram],
        "icu_normalizer" => vec![Step::IcuNormalize(unicode_set_of(spec))],
        "icu_folding" => vec![Step::IcuFold(unicode_set_of(spec))],
        "icu_collation" | "icu_collation_keyword" => vec![Step::Collate {
            strength: match spec.get("strength").and_then(|v| v.as_str()).unwrap_or("tertiary") {
                "primary" => Strength::Primary,
                "secondary" => Strength::Secondary,
                _ => Strength::Tertiary,
            },
        }],
        "kuromoji_baseform" => vec![Step::BaseForm(morph::Language::Japanese)],
        "kuromoji_part_of_speech" => vec![Step::PartOfSpeech {
            language: morph::Language::Japanese,
            stoptags: stoptags_of(spec),
        }],
        "kuromoji_readingform" => vec![Step::Reading(morph::Language::Japanese)],
        "kuromoji_stemmer" => vec![Step::KatakanaStem {
            minimum: spec
                .get("minimum_length")
                .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
                .unwrap_or(4) as usize,
        }],
        "kuromoji_completion" => vec![Step::Completion {
            index: spec.get("mode").and_then(|v| v.as_str()).unwrap_or("index") == "index",
        }],
        "nori_part_of_speech" => vec![Step::PartOfSpeech {
            language: morph::Language::Korean,
            stoptags: stoptags_of(spec),
        }],
        "nori_readingform" => vec![Step::Reading(morph::Language::Korean)],
        "nori_number" => vec![Step::KoreanNumber],
        "keyword_marker" => {
            vec![Step::KeywordMarker(spec.get("keywords").map(word_list).unwrap_or_default())]
        }
        "stemmer_override" => {
            let mut told = HashMap::new();
            for rule in spec.get("rules").and_then(|r| r.as_array()).cloned().unwrap_or_default() {
                if let Some((from, to)) = rule.as_str().and_then(|r| r.split_once("=>")) {
                    for word in from.split(',') {
                        told.insert(word.trim().to_lowercase(), to.trim().to_string());
                    }
                }
            }
            vec![Step::StemmerOverride(told)]
        }
        "dictionary_decompounder" | "hyphenation_decompounder" => {
            vec![Step::Decompound(spec.get("word_list").map(word_list).unwrap_or_default())]
        }
        "pattern_capture" => vec![Step::PatternCapture {
            patterns: spec.get("patterns").map(word_list).unwrap_or_default(),
            preserve_original: spec
                .get("preserve_original")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
        }],
        "pattern_replace" => vec![Step::PatternReplace {
            pattern: text("pattern").unwrap_or("").to_string(),
            replacement: text("replacement").unwrap_or("").to_string(),
        }],
        "keep" => vec![Step::Keep {
            words: spec.get("keep_words").map(word_list).unwrap_or_default(),
            keep: true,
        }],
        "keep_types" => vec![Step::KeepTypes {
            types: spec.get("types").map(word_list).unwrap_or_default(),
            keep: text("mode").map(|m| m != "exclude").unwrap_or(true),
        }],
        "word_delimiter" | "word_delimiter_graph" => vec![Step::WordDelimiter {
            catenate: flag("catenate_all") || flag("catenate_words"),
            adjust_offsets: spec.get("adjust_offsets").and_then(|v| v.as_bool()).unwrap_or(true),
            on_numerics: spec.get("split_on_numerics").and_then(|v| v.as_bool()).unwrap_or(true),
            on_case_change: spec
                .get("split_on_case_change")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
        }],
        "common_grams" => {
            vec![Step::CommonGrams(spec.get("common_words").map(word_list).unwrap_or_default())]
        }
        "shingle" => {
            let min = num("min_shingle_size", 2);
            let max = num("max_shingle_size", 2);
            let text = |key: &str, fallback: &str| {
                spec.get(key)
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| fallback.to_string())
            };
            vec![Step::Shingle {
                min,
                max,
                unigrams: spec.get("output_unigrams").and_then(|v| v.as_bool()).unwrap_or(true),
                unigrams_if_none: spec
                    .get("output_unigrams_if_no_shingles")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                separator: text("token_separator", " "),
                filler: text("filler_token", "_"),
            }]
        }
        "delimited_term_freq" => vec![Step::DelimitedTermFreq(one("delimiter", '|'))],
        "delimited_payload" => vec![Step::DelimitedPayload(one("delimiter", '|'))],
        "ngram" => vec![Step::NgramTokens {
            min: num("min_gram", 1),
            max: num("max_gram", 2),
            edges: false,
        }],
        "edge_ngram" => vec![Step::NgramTokens {
            min: num("min_gram", 1),
            max: num("max_gram", 2),
            edges: true,
        }],
        "multiplexer" => {
            let branches = spec
                .get("filters")
                .and_then(|f| f.as_array())
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|f| f.as_str())
                .map(|line| {
                    line.split(',')
                        .filter_map(|one| token_filter(one.trim(), defined))
                        .flatten()
                        .collect::<Vec<Step>>()
                })
                .collect();
            vec![Step::Multiplexer(branches)]
        }
        "min_hash" => {
            vec![Step::MinHash { buckets: num("bucket_count", 512), hashes: num("hash_count", 1) }]
        }
        "flatten_graph" => vec![Step::FlattenGraph],
        "type_as_payload" => vec![Step::TypeAsPayload],
        "remove_duplicates" => vec![],
        other => return filter_of_name(other),
    };
    Some(steps)
}

/// A filter named rather than described.
fn filter_of_name(name: &str) -> Option<Vec<Step>> {
    Some(match name {
        "lowercase" => vec![Step::Lowercase],
        "uppercase" => vec![Step::Uppercase],
        "asciifolding" => vec![Step::AsciiFolding],
        "trim" => vec![Step::Trim],
        "reverse" => vec![Step::Reverse],
        "unique" => vec![Step::Unique],
        "elision" => vec![Step::Elision],
        "apostrophe" => vec![Step::Apostrophe],
        "classic" => vec![Step::Classic],
        "decimal_digit" => vec![Step::DecimalDigits],
        "cjk_width" => vec![Step::CjkWidth],
        "cjk_bigram" => vec![Step::CjkBigram],
        "delimited_payload" => vec![Step::DelimitedPayload('|')],
        "delimited_term_freq" => vec![Step::DelimitedTermFreq('|')],
        "stop" => vec![Step::Stop(stop_words("_english_"))],
        "kstem" => vec![Step::KStem],
        "porter_stem" | "porterStem" => vec![Step::Stem("porter".into())],
        "snowball" => vec![Step::Stem("english".into())],
        "fingerprint" => vec![Step::Fingerprint(' ')],
        "word_delimiter" | "word_delimiter_graph" => {
            vec![Step::WordDelimiter {
                catenate: false,
                on_numerics: true,
                on_case_change: true,
                adjust_offsets: true,
            }]
        }
        "min_hash" => vec![Step::MinHash { buckets: 512, hashes: 1 }],
        "shingle" => vec![Step::Shingle {
            min: 2,
            max: 2,
            unigrams: true,
            unigrams_if_none: false,
            separator: " ".to_string(),
            filler: "_".to_string(),
        }],
        "type_as_payload" => vec![Step::TypeAsPayload],
        "flatten_graph" => vec![Step::FlattenGraph],
        "remove_duplicates" => vec![],
        "keyword_repeat" => vec![Step::KeywordRepeat],
        "icu_normalizer" => vec![Step::IcuNormalize(None)],
        "icu_folding" => vec![Step::IcuFold(None)],
        "icu_collation" => vec![Step::Collate { strength: Strength::Tertiary }],
        "kuromoji_baseform" => vec![Step::BaseForm(morph::Language::Japanese)],
        "kuromoji_part_of_speech" => {
            vec![Step::PartOfSpeech { language: morph::Language::Japanese, stoptags: None }]
        }
        "kuromoji_readingform" => vec![Step::Reading(morph::Language::Japanese)],
        "kuromoji_stemmer" => vec![Step::KatakanaStem { minimum: 4 }],
        "kuromoji_completion" => vec![Step::Completion { index: true }],
        "nori_part_of_speech" => {
            vec![Step::PartOfSpeech { language: morph::Language::Korean, stoptags: None }]
        }
        "nori_readingform" => vec![Step::Reading(morph::Language::Korean)],
        "nori_number" => vec![Step::KoreanNumber],
        "arabic_normalization" => vec![Step::Normalize("arabic")],
        "bengali_normalization" => vec![Step::Normalize("bengali")],
        "german_normalization" => vec![Step::Normalize("german")],
        "hindi_normalization" => vec![Step::Normalize("hindi")],
        "indic_normalization" => vec![Step::Normalize("indic")],
        "persian_normalization" => vec![Step::Normalize("persian")],
        "sorani_normalization" => vec![Step::Normalize("sorani")],
        "serbian_normalization" => vec![Step::Normalize("serbian")],
        "scandinavian_normalization" => vec![Step::Normalize("scandinavian")],
        "scandinavian_folding" => vec![Step::Normalize("scandinavian_folding")],
        // a filter that names a language stems it
        other => {
            let language = other.strip_suffix("_stem")?;
            vec![Step::Stem(language.to_string())]
        }
    })
}

fn word_list(list: &Value) -> Vec<String> {
    match list {
        // `_english_` names a list OpenSearch keeps; anything else is the
        // words themselves, written out
        Value::String(s) if s.starts_with('_') && s.ends_with('_') => stop_words(s),
        Value::String(s) => {
            s.split(',').map(|w| w.trim().to_string()).filter(|w| !w.is_empty()).collect()
        }
        Value::Array(a) => a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
        _ => Vec::new(),
    }
}

/// A rule written with escapes in it -- `\\u0020` for a space -- as the text
/// it stands for.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && chars.get(i + 1) == Some(&'u') && i + 5 < chars.len() {
            let code: String = chars[i + 2..i + 6].iter().collect();
            if let Ok(n) = u32::from_str_radix(&code, 16)
                && let Some(c) = char::from_u32(n)
            {
                out.push(c);
                i += 6;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// The char filters a spec names, read against the ones the index defined.
fn char_filters_of(named: &[Value], defined: &Value) -> Vec<CharFilter> {
    let registry = Registry {
        named: HashMap::new(),
        tokenizers: Value::Null,
        filters: Value::Null,
        char_filters: defined.clone(),
    };
    registry.char_filters(named)
}

/// `"a, b => c"` and `"a, b"`, which is how synonyms are written.
fn synonyms(spec: &Value, graph: bool) -> Vec<SynonymRule> {
    let mut rules = Vec::new();
    let lines: Vec<String> = match spec.get("synonyms") {
        Some(Value::Array(a)) => {
            a.iter().filter_map(|l| l.as_str().map(|s| s.to_string())).collect()
        }
        Some(Value::String(s)) => s.lines().map(|l| l.to_string()).collect(),
        _ => Vec::new(),
    };
    let words = |text: &str| -> Vec<String> {
        text.split_whitespace().map(|w| w.trim().to_lowercase()).filter(|w| !w.is_empty()).collect()
    };
    for line in lines {
        match line.split_once("=>") {
            // `a, b => c` says what a and b are to be read as
            Some((from, to)) => {
                let targets: Vec<Vec<String>> =
                    to.split(',').map(words).filter(|w| !w.is_empty()).collect();
                for phrase in from.split(',').map(words).filter(|w| !w.is_empty()) {
                    rules.push(SynonymRule {
                        phrase,
                        alternatives: targets.clone(),
                        keep_original: false,
                        alternatives_first: true,
                        graph,
                    });
                }
            }
            // `a, b, c` says the three are the same word
            None => {
                let group: Vec<Vec<String>> =
                    line.split(',').map(words).filter(|w| !w.is_empty()).collect();
                for phrase in &group {
                    let alternatives: Vec<Vec<String>> =
                        group.iter().filter(|other| *other != phrase).cloned().collect();
                    rules.push(SynonymRule {
                        phrase: phrase.clone(),
                        alternatives,
                        keep_original: true,
                        // a graph filter reads what the word also means before
                        // the word itself; the plain one reads it after
                        alternatives_first: graph,
                        graph,
                    });
                }
            }
        }
    }
    rules
}

/// One synonym rule: the words it stands for, and what may be read in their
/// place.
#[derive(Clone, Debug)]
pub struct SynonymRule {
    phrase: Vec<String>,
    alternatives: Vec<Vec<String>>,
    /// whether the words written are kept beside what they also mean
    keep_original: bool,
    /// whether what they also mean is read first
    alternatives_first: bool,
    /// whether the ways of reading it are laid out as paths through a graph
    /// rather than stacked in place
    graph: bool,
}

/// The analyzers OpenSearch has without being told about them.
pub fn builtin(name: &str) -> Option<Chain> {
    // what a language analyzer is: cut into words, lowercased, its own stop
    // words dropped, and what is left cut down to its stem
    let lang = |l: &str| Chain {
        pre: Vec::new(),
        source: Source::Standard,
        steps: vec![Step::Lowercase, Step::Stop(stop_words(l)), Step::Stem(l.to_string())],
        annotated: false,
    };
    // the languages whose stemmer wants the word written one way first
    let normalized = |l: &str, first: Step| Chain {
        pre: Vec::new(),
        source: Source::Standard,
        steps: vec![Step::Lowercase, first, Step::Stop(stop_words(l)), Step::Stem(l.to_string())],
        annotated: false,
    };
    Some(match name {
        "standard" | "default" => Chain {
            pre: Vec::new(),
            source: Source::Standard,
            steps: vec![Step::Lowercase],
            annotated: false,
        },
        "simple" => Chain {
            pre: Vec::new(),
            source: Source::Letter,
            steps: vec![Step::Lowercase],
            annotated: false,
        },
        "whitespace" => {
            Chain { pre: Vec::new(), source: Source::Whitespace, steps: vec![], annotated: false }
        }
        "stop" => Chain {
            pre: Vec::new(),
            source: Source::Letter,
            steps: vec![Step::Lowercase, Step::Stop(stop_words("_english_"))],
            annotated: false,
        },
        "keyword" | "raw" => {
            Chain { pre: Vec::new(), source: Source::Keyword, steps: vec![], annotated: false }
        }
        // the index keeps every prefix a number may be typed as; the search
        // keeps the number alone, so that what was typed is matched against
        // whole numbers rather than against every number that begins with it
        "phone" | "phone-search" => Chain {
            pre: Vec::new(),
            source: Source::Phone { region: "ZZ".into(), ngrams: name == "phone" },
            steps: Vec::new(),
            annotated: false,
        },
        "pattern" => Chain {
            pre: Vec::new(),
            source: Source::PatternSplit(r"[^a-zA-Z0-9_]+".into()),
            steps: vec![Step::Lowercase],
            annotated: false,
        },
        "fingerprint" => Chain {
            pre: Vec::new(),
            source: Source::Standard,
            steps: vec![Step::Lowercase, Step::AsciiFolding, Step::Fingerprint(' ')],
            annotated: false,
        },
        "en_stem" => lang("english"),
        // English drops the possessive before it stems, and stems with the
        // algorithm Porter first wrote
        "english" => Chain {
            pre: Vec::new(),
            source: Source::Standard,
            steps: vec![
                Step::Possessive,
                Step::Lowercase,
                Step::Stop(stop_words("_english_")),
                Step::Stem("porter".into()),
            ],
            annotated: false,
        },
        // a Snowball analyzer is the English one under the name of the
        // algorithm it runs
        "snowball" => lang("english"),
        // what OpenSearch keeps for indices made long ago: words and stop
        // words, and no stemming at all
        "chinese" => Chain {
            pre: Vec::new(),
            source: Source::Standard,
            steps: vec![Step::Lowercase, Step::Stop(stop_words("_english_"))],
            annotated: false,
        },
        // Chinese, Japanese and Korean are not written with spaces between
        // words, so a pair of characters stands in for one
        "cjk" => Chain {
            pre: Vec::new(),
            source: Source::Standard,
            steps: vec![
                Step::CjkWidth,
                Step::Lowercase,
                Step::CjkBigram,
                Step::Stop(stop_words("_english_")),
            ],
            annotated: false,
        },
        // the languages whose stemmer is a light one, or wants the word
        // written its way first
        "french" => Chain {
            pre: Vec::new(),
            source: Source::Standard,
            steps: vec![
                Step::Elision,
                Step::Lowercase,
                Step::Stop(stop_words("french")),
                Step::Stem("french_light".into()),
            ],
            annotated: false,
        },
        "italian" => Chain {
            pre: Vec::new(),
            source: Source::Standard,
            steps: vec![
                Step::Elision,
                Step::Lowercase,
                Step::Stop(stop_words("italian")),
                Step::Stem("italian_light".into()),
            ],
            annotated: false,
        },
        "spanish" => Chain {
            pre: Vec::new(),
            source: Source::Standard,
            steps: vec![
                Step::Lowercase,
                Step::Stop(stop_words("spanish")),
                Step::Stem("spanish_light".into()),
            ],
            annotated: false,
        },
        "portuguese" => Chain {
            pre: Vec::new(),
            source: Source::Standard,
            steps: vec![
                Step::Lowercase,
                Step::Stop(stop_words("portuguese")),
                Step::Stem("portuguese_light".into()),
            ],
            annotated: false,
        },
        "irish" => Chain {
            pre: Vec::new(),
            source: Source::Standard,
            steps: vec![
                Step::Elision,
                Step::IrishLowercase,
                Step::Stop(stop_words("irish")),
                Step::Stem("irish".into()),
            ],
            annotated: false,
        },
        "catalan" => Chain {
            pre: Vec::new(),
            source: Source::Standard,
            steps: vec![
                Step::Elision,
                Step::Lowercase,
                Step::Stop(stop_words("catalan")),
                Step::Stem("catalan".into()),
            ],
            annotated: false,
        },
        "greek" => Chain {
            pre: Vec::new(),
            source: Source::Standard,
            steps: vec![
                Step::GreekLowercase,
                Step::Stop(stop_words("greek")),
                Step::Stem("greek".into()),
            ],
            annotated: false,
        },
        "persian" => Chain {
            // the joiner Persian writes inside a word is not part of any word:
            // what stands either side of it is two words
            pre: vec![CharFilter::Mapping(vec![("\u{200c}".to_string(), " ".to_string())])],
            source: Source::Standard,
            steps: vec![
                Step::Lowercase,
                Step::DecimalDigits,
                Step::Normalize("arabic"),
                Step::PersianNormalize,
                // the words to drop are compared with words that have been
                // written the one way, so they are written that way too
                Step::Stop(
                    stop_words("persian")
                        .iter()
                        .map(|w| stem::persian_normalize(&stem::normalize("arabic", w)))
                        .collect(),
                ),
                Step::Stem("persian".into()),
            ],
            annotated: false,
        },
        "thai" => Chain {
            pre: Vec::new(),
            source: Source::Thai,
            steps: vec![Step::Lowercase, Step::DecimalDigits],
            annotated: false,
        },
        "sorani" => Chain {
            pre: Vec::new(),
            source: Source::Standard,
            steps: vec![
                Step::Lowercase,
                Step::Stop(stop_words("sorani")),
                Step::Stem("sorani".into()),
            ],
            annotated: false,
        },
        "romanian" => normalized("romanian", Step::RomanianNormalize),
        "turkish" => Chain {
            pre: Vec::new(),
            source: Source::Standard,
            steps: vec![
                Step::Apostrophe,
                Step::Lowercase,
                Step::Stop(stop_words("turkish")),
                Step::Stem("turkish".into()),
            ],
            annotated: false,
        },
        "german" => Chain {
            pre: Vec::new(),
            source: Source::Standard,
            steps: vec![
                Step::Lowercase,
                Step::Stop(stop_words("german")),
                Step::Normalize("german"),
                Step::Stem("german_light".into()),
            ],
            annotated: false,
        },
        // Japanese: the words a dictionary finds, each as it stands on its
        // own, without the particles and endings a search has no use for
        "kuromoji" => Chain {
            pre: Vec::new(),
            source: Source::Morph {
                language: morph::Language::Japanese,
                drop_grammar: true,
                base_form: true,
                search: true,
            },
            steps: vec![Step::Lowercase],
            annotated: false,
        },
        // the same words, each one followed by how it is typed on a Latin
        // keyboard, for a search box that completes as somebody types
        "kuromoji_completion" => Chain {
            pre: Vec::new(),
            source: Source::Morph {
                language: morph::Language::Japanese,
                drop_grammar: false,
                base_form: false,
                search: true,
            },
            steps: vec![Step::Completion { index: true }],
            annotated: false,
        },
        "nori" => Chain {
            pre: Vec::new(),
            source: Source::Morph {
                language: morph::Language::Korean,
                drop_grammar: true,
                base_form: false,
                search: false,
            },
            steps: vec![Step::Lowercase],
            annotated: false,
        },
        "smartcn" => Chain {
            pre: Vec::new(),
            source: Source::Morph {
                language: morph::Language::Chinese,
                drop_grammar: true,
                base_form: false,
                search: false,
            },
            steps: vec![Step::Lowercase],
            annotated: false,
        },
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
