//! How text becomes the tokens an index holds.
//!
//! OpenSearch calls this an analyzer: a character filter, a tokenizer and a
//! chain of token filters. A field names one, an index may define its own, and
//! a search has to cut the query the same way the document was cut or the two
//! never meet.
//!
//! Everything here is built from what the index settings say, and handed to
//! BoostCore as a `TextAnalyzer` under the name the mapping uses.

use std::collections::HashMap;

use boostcore::tokenizer::{
    AsciiFoldingFilter, Language, NgramTokenizer, RawTokenizer, RegexTokenizer, RemoveLongFilter,
    SimpleTokenizer, Stemmer, TextAnalyzer, WhitespaceTokenizer,
};
use serde_json::Value;

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
    Ngram { min: usize, max: usize, edges: bool },
}

/// One step of a chain, in the order OpenSearch writes them.
#[derive(Clone, Debug)]
enum Step {
    Lowercase,
    AsciiFolding,
    Stop(Vec<String>),
    Stem(String),
    Length { min: usize, max: usize },
    Trim,
    Reverse,
    Unique,
    Truncate(usize),
    Limit(usize),
    /// each token replaced by, or joined with, what it maps to
    Synonym(HashMap<String, Vec<String>>),
    /// sorted, deduplicated and joined back into one token
    Fingerprint,
}

/// A named analysis chain.
#[derive(Clone, Debug)]
pub struct Chain {
    source: Source,
    steps: Vec<Step>,
}

impl Chain {
    /// The tokens this chain makes of a text, with where each came from.
    pub fn tokens(&self, text: &str) -> Vec<(String, usize, usize, usize)> {
        let mut out: Vec<(String, usize, usize, usize)> = Vec::new();
        // the tokenizer runs inside BoostCore; the steps that it also has run
        // there too, and the rest are applied here
        let mut analyzer = self.boostcore_analyzer();
        let mut stream = analyzer.token_stream(text);
        while stream.advance() {
            let t = stream.token();
            out.push((t.text.clone(), t.position, t.offset_from, t.offset_to));
        }
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
            Source::Letter => TextAnalyzer::builder(SimpleTokenizer::default()).dynamic(),
            Source::Whitespace => TextAnalyzer::builder(WhitespaceTokenizer::default()).dynamic(),
            Source::Keyword => TextAnalyzer::builder(RawTokenizer::default()).dynamic(),
            Source::Pattern(p) => match RegexTokenizer::new(p) {
                Ok(t) => TextAnalyzer::builder(t).dynamic(),
                Err(_) => TextAnalyzer::builder(SimpleTokenizer::default()).dynamic(),
            },
            Source::Ngram { min, max, edges } => {
                match NgramTokenizer::new(*min, *max, *edges) {
                    Ok(t) => TextAnalyzer::builder(t).dynamic(),
                    Err(_) => TextAnalyzer::builder(SimpleTokenizer::default()).dynamic(),
                }
            }
        };
        // a token longer than a term may be is dropped, as it is upstream
        base.filter_dynamic(RemoveLongFilter::limit(255)).build()
    }

    /// Whether a chain leaves tokens the JSON field's own tokenizer would cut
    /// again. Those cannot be written through the pre-analysed path.
    pub fn splits_further(&self, tokens: &[String]) -> bool {
        tokens.iter().any(|t| t.chars().any(|c| !c.is_alphanumeric()))
    }
}

/// Steps BoostCore has no filter for, or where OpenSearch's order differs.
fn apply_here(
    step: &Step,
    tokens: Vec<(String, usize, usize, usize)>,
) -> Vec<(String, usize, usize, usize)> {
    match step {
        Step::Lowercase => tokens
            .into_iter()
            .map(|(t, p, a, b)| (t.to_lowercase(), p, a, b))
            .collect(),
        Step::AsciiFolding => tokens
            .into_iter()
            .map(|(t, p, a, b)| (fold_to_ascii(&t), p, a, b))
            .collect(),
        Step::Stop(words) => {
            let set: std::collections::HashSet<String> =
                words.iter().map(|w| w.to_lowercase()).collect();
            tokens.into_iter().filter(|(t, _, _, _)| !set.contains(&t.to_lowercase())).collect()
        }
        Step::Stem(lang) => match language(lang) {
            Some(l) => {
                let mut analyzer =
                    TextAnalyzer::builder(RawTokenizer::default()).filter(Stemmer::new(l)).build();
                tokens
                    .into_iter()
                    .map(|(t, p, a, b)| {
                        let stemmed = {
                            let mut s = analyzer.token_stream(&t);
                            if s.advance() { s.token().text.clone() } else { t.clone() }
                        };
                        (stemmed, p, a, b)
                    })
                    .collect()
            }
            None => tokens,
        },
        Step::Length { min, max } => tokens
            .into_iter()
            .filter(|(t, _, _, _)| t.chars().count() >= *min && t.chars().count() <= *max)
            .collect(),
        Step::Trim => tokens
            .into_iter()
            .map(|(t, p, a, b)| (t.trim().to_string(), p, a, b))
            .filter(|(t, _, _, _)| !t.is_empty())
            .collect(),
        Step::Reverse => tokens
            .into_iter()
            .map(|(t, p, a, b)| (t.chars().rev().collect(), p, a, b))
            .collect(),
        Step::Unique => {
            let mut seen = std::collections::HashSet::new();
            tokens.into_iter().filter(|(t, _, _, _)| seen.insert(t.clone())).collect()
        }
        Step::Truncate(n) => tokens
            .into_iter()
            .map(|(t, p, a, b)| (t.chars().take(*n).collect(), p, a, b))
            .collect(),
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
    }
}

/// Latin letters with a mark, written without it.
fn fold_to_ascii(text: &str) -> String {
    let mut analyzer =
        TextAnalyzer::builder(RawTokenizer::default()).filter(AsciiFoldingFilter).build();
    let mut stream = analyzer.token_stream(text);
    if stream.advance() {
        stream.token().text.clone()
    } else {
        text.to_string()
    }
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
            "au", "aux", "avec", "ce", "ces", "dans", "de", "des", "du", "elle", "en", "et",
            "eux", "il", "je", "la", "le", "les", "leur", "lui", "ma", "mais", "me", "même",
            "mes", "moi", "mon", "ne", "nos", "notre", "nous", "on", "ou", "par", "pas", "pour",
            "qu", "que", "qui", "sa", "se", "ses", "son", "sur", "ta", "te", "tes", "toi", "ton",
            "tu", "un", "une", "vos", "votre", "vous",
        ],
        "_german_" | "german" => &[
            "aber", "alle", "als", "also", "am", "an", "auch", "auf", "aus", "bei", "bin", "bis",
            "da", "das", "dass", "dem", "den", "der", "des", "die", "doch", "du", "ein", "eine",
            "er", "es", "für", "hat", "ich", "ihr", "im", "in", "ist", "mit", "nicht", "noch",
            "nur", "oder", "sich", "sie", "sind", "über", "um", "und", "von", "vor", "war",
            "was", "wenn", "werden", "wie", "wir", "zu", "zum", "zur",
        ],
        "_spanish_" | "spanish" => &[
            "a", "al", "algo", "como", "con", "de", "del", "el", "en", "entre", "era", "es",
            "esta", "este", "ha", "hay", "la", "las", "le", "lo", "los", "más", "me", "mi", "no",
            "o", "para", "pero", "por", "que", "se", "si", "sin", "sobre", "su", "sus", "también",
            "te", "tu", "un", "una", "uno", "y", "ya",
        ],
        "_italian_" | "italian" => &[
            "a", "ad", "al", "alla", "che", "chi", "come", "con", "da", "del", "della", "di",
            "e", "ed", "il", "in", "la", "le", "lo", "ma", "mi", "ne", "non", "per", "più",
            "quale", "se", "si", "sono", "su", "sul", "una", "uno",
        ],
        "_portuguese_" | "portuguese" => &[
            "a", "ao", "aos", "as", "com", "como", "da", "das", "de", "do", "dos", "e", "em",
            "for", "isso", "já", "mais", "mas", "me", "na", "nas", "no", "nos", "não", "o", "os",
            "ou", "para", "pela", "pelo", "por", "que", "se", "sem", "ser", "seu", "sua", "são",
            "também", "um", "uma",
        ],
        "_russian_" | "russian" => &[
            "а", "без", "более", "бы", "был", "была", "были", "было", "быть", "в", "вам", "вас",
            "весь", "во", "вот", "все", "всего", "всех", "вы", "где", "да", "даже", "для", "до",
            "его", "ее", "если", "есть", "еще", "же", "за", "здесь", "и", "из", "или", "им",
            "их", "к", "как", "ко", "когда", "кто", "ли", "либо", "мне", "может", "мы", "на",
            "надо", "наш", "не", "него", "нее", "нет", "ни", "них", "но", "ну", "о", "об",
            "они", "оно", "от", "очень", "по", "под", "при", "с", "со", "так", "также", "такой",
            "там", "те", "тем", "то", "того", "тоже", "той", "только", "том", "ты", "у", "уже",
            "хотя", "чего", "чей", "чем", "что", "чтобы", "чье", "чья", "эта", "эти", "это",
            "я",
        ],
        "_arabic_" | "arabic" => &[
            "من", "في", "على", "و", "أن", "إلى", "عن", "ما", "هذا", "هذه", "التي", "الذي",
        ],
        _ => &[],
    };
    list.iter().map(|s| s.to_string()).collect()
}

/// The analyzers an index can name, whether or not it defined any.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    named: HashMap<String, Chain>,
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
            return registry;
        };
        for (name, spec) in defined {
            if let Some(chain) = build(spec, &tokenizers, &filters) {
                registry.named.insert(name.clone(), chain);
            }
        }
        registry
    }

    /// The chain a name stands for: the index's own first, then the built-ins.
    pub fn get(&self, name: &str) -> Option<Chain> {
        self.named.get(name).cloned().or_else(|| builtin(name))
    }

    /// Whether this name means anything at all.
    pub fn knows(&self, name: &str) -> bool {
        self.named.contains_key(name) || builtin(name).is_some()
    }

    pub fn names(&self) -> Vec<String> {
        self.named.keys().cloned().collect()
    }
}

/// An analyzer the index defined, out of the parts it named.
fn build(spec: &Value, tokenizers: &Value, filters: &Value) -> Option<Chain> {
    // `{"type": "english"}` names a built-in rather than describing a chain
    if let Some(kind) = spec.get("type").and_then(|t| t.as_str()) {
        if kind != "custom" {
            if let Some(mut chain) = builtin(kind) {
                if let Some(list) = spec.get("stopwords") {
                    let words = word_list(list);
                    chain.steps.retain(|s| !matches!(s, Step::Stop(_)));
                    chain.steps.push(Step::Stop(words));
                }
                return Some(chain);
            }
        }
    }
    let named = spec.get("tokenizer").and_then(|t| t.as_str()).unwrap_or("standard");
    let source = tokenizer_source(named, tokenizers);
    let mut steps = Vec::new();
    for step in spec.get("filter").into_iter().flat_map(|f| f.as_array().cloned().unwrap_or_default())
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
        let kind = spec.get("type").and_then(|t| t.as_str()).unwrap_or("standard");
        let num = |k: &str, d: usize| {
            spec.get(k).and_then(|v| v.as_u64()).map(|v| v as usize).unwrap_or(d)
        };
        return match kind {
            "pattern" | "simple_pattern" | "simple_pattern_split" => Source::Pattern(
                spec.get("pattern").and_then(|p| p.as_str()).unwrap_or(r"\w+").to_string(),
            ),
            "ngram" => Source::Ngram { min: num("min_gram", 1), max: num("max_gram", 2), edges: false },
            "edge_ngram" => {
                Source::Ngram { min: num("min_gram", 1), max: num("max_gram", 2), edges: true }
            }
            "keyword" => Source::Keyword,
            "whitespace" => Source::Whitespace,
            "letter" => Source::Letter,
            _ => Source::Standard,
        };
    }
    match name {
        "keyword" => Source::Keyword,
        "whitespace" => Source::Whitespace,
        "letter" | "lowercase" => Source::Letter,
        "pattern" => Source::Pattern(r"\w+".into()),
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
                let targets: Vec<String> =
                    to.split(',').map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()).collect();
                for word in from.split(',').map(|w| w.trim().to_lowercase()) {
                    if !word.is_empty() {
                        map.insert(word, targets.clone());
                    }
                }
            }
            None => {
                let group: Vec<String> =
                    line.split(',').map(|w| w.trim().to_lowercase()).filter(|w| !w.is_empty()).collect();
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
    let lang = |l: &str| Chain {
        source: Source::Standard,
        steps: vec![
            Step::Lowercase,
            Step::Stop(stop_words(l)),
            Step::Stem(l.to_string()),
        ],
    };
    Some(match name {
        "standard" | "default" => {
            Chain { source: Source::Standard, steps: vec![Step::Lowercase] }
        }
        "simple" => Chain { source: Source::Letter, steps: vec![Step::Lowercase] },
        "whitespace" => Chain { source: Source::Whitespace, steps: vec![] },
        "stop" => Chain {
            source: Source::Letter,
            steps: vec![Step::Lowercase, Step::Stop(stop_words("_english_"))],
        },
        "keyword" | "raw" => Chain { source: Source::Keyword, steps: vec![] },
        "pattern" => {
            Chain { source: Source::Pattern(r"\w+".into()), steps: vec![Step::Lowercase] }
        }
        "fingerprint" => Chain {
            source: Source::Standard,
            steps: vec![Step::Lowercase, Step::AsciiFolding, Step::Fingerprint],
        },
        "en_stem" => lang("english"),
        other => {
            language(other)?;
            lang(other)
        }
    })
}
