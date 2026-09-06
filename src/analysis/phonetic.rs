//! Words written as they sound.
//!
//! The encoders are the Apache commons-codec ones, which is what OpenSearch's
//! phonetic plugin uses, so a word encoded here is the word encoded there.

use std::sync::OnceLock;

use rphonetic::{
    BeiderMorse, BeiderMorseBuilder, Caverphone1, Caverphone2, Cologne, ConfigFiles,
    DaitchMokotoffSoundex, DaitchMokotoffSoundexBuilder, DoubleMetaphone, Encoder, LanguageSet,
    Metaphone, NameType, Nysiis, RefinedSoundex, RuleType, Soundex,
};

/// What a `phonetic` filter was told to do.
pub struct How<'a> {
    pub encoder: &'a str,
    pub languages: &'a [String],
    pub max_code_len: Option<usize>,
    pub name_type: &'a str,
    pub rule_type: &'a str,
}

fn daitch() -> Option<&'static DaitchMokotoffSoundex> {
    static D: OnceLock<Option<DaitchMokotoffSoundex>> = OnceLock::new();
    D.get_or_init(|| DaitchMokotoffSoundexBuilder::default().build().ok()).as_ref()
}

/// The Beider-Morse rule files.
///
/// The set built into the library covers the language-agnostic rules only.
/// Naming a language -- `languageset: polish` -- needs the per-language files
/// Apache commons-codec ships, which are looked for in a directory beside the
/// other analysis data. Where that is, and what happens without it, is in
/// `docs/phonetic.md`.
fn beider_morse_rules() -> Option<&'static ConfigFiles> {
    static C: OnceLock<Option<ConfigFiles>> = OnceLock::new();
    C.get_or_init(|| {
        for dir in rule_dirs() {
            if dir.is_dir()
                && let Ok(files) = ConfigFiles::new(&dir)
            {
                return Some(files);
            }
        }
        Some(ConfigFiles::default())
    })
    .as_ref()
}

fn rule_dirs() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(d) = std::env::var("BOOSTSEARCH_PHONETIC_RULES") {
        out.push(std::path::PathBuf::from(d));
    }
    for base in [std::env::var("BOOSTSEARCH_CONFIG").ok(), std::env::var("BOOSTSEARCH_DATA").ok()]
        .into_iter()
        .flatten()
    {
        out.push(std::path::PathBuf::from(&base).join("analysis-phonetic"));
        out.push(std::path::PathBuf::from(&base).join("config").join("analysis-phonetic"));
    }
    out.push(std::path::PathBuf::from("config").join("analysis-phonetic"));
    out
}

/// One word, written as the named encoder hears it.
///
/// `None` means the encoder had nothing to say about the word -- which is not
/// the same as an empty code, and leaves the word standing on its own.
pub fn encode(how: &How, word: &str) -> Option<String> {
    // The rule sets these encoders read are data, and a combination no rule
    // was written for makes the library give up where it stands. A token is
    // not worth a node, so a word that does that is a word left alone.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| encode_inner(how, word)))
        .unwrap_or(None)
}

fn encode_inner(how: &How, word: &str) -> Option<String> {
    let How { encoder, languages, max_code_len, name_type, rule_type } = *how;
    let code = match encoder {
        "metaphone" => Metaphone::new(max_code_len).encode(word),
        // four characters unless the filter asked for more, which is what
        // OpenSearch's `max_code_len` defaults to
        "double_metaphone" | "doublemetaphone" => {
            DoubleMetaphone::new(Some(max_code_len.unwrap_or(4))).encode(word)
        }
        "soundex" => Soundex::default().encode(word),
        "refined_soundex" | "refinedsoundex" => RefinedSoundex::default().encode(word),
        "caverphone1" => Caverphone1.encode(word),
        "caverphone" | "caverphone2" => Caverphone2.encode(word),
        "cologne" | "koelnerphonetik" => Cologne.encode(word),
        "nysiis" => Nysiis::default().encode(word),
        "daitch_mokotoff" | "daitchmokotoffsoundex" => daitch()?.encode(word),
        "beider_morse" | "beidermorse" => {
            let rules = beider_morse_rules()?;
            let builder = BeiderMorseBuilder::new(rules)
                .name_type(match name_type {
                    "ashkenazi" => NameType::Ashkenazi,
                    "sephardic" => NameType::Sephardic,
                    _ => NameType::Generic,
                })
                .rule_type(match rule_type {
                    "exact" => RuleType::Exact,
                    _ => RuleType::Approx,
                });
            let bm: BeiderMorse = builder.build();
            // a language named on the filter narrows the rules to that
            // language's; naming none leaves the encoder to guess
            if languages.is_empty() {
                bm.encode(word)
            } else {
                let set =
                    LanguageSet::from(languages.iter().map(|l| l.as_str()).collect::<Vec<&str>>());
                bm.encode_with_languages(word, &set)
            }
        }
        _ => return None,
    };
    (!code.is_empty()).then_some(code)
}
