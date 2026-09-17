//! Cutting a long text into pieces small enough to embed.
//!
//! A model that turns text into a vector has a window, and a document longer
//! than the window cannot be embedded whole. So the text is cut first, and
//! each piece embedded on its own. Three ways of cutting are named: a fixed
//! number of tokens, a fixed number of characters, or wherever a delimiter
//! stands.
//!
//! Every piece is a slice of the text as it was written, not a rebuilt
//! string: the space between two words belongs to the piece the earlier word
//! is in, so putting the pieces back together gives the text back. A piece
//! therefore runs from where its first token begins to where the next
//! piece's first token begins, and the first piece runs from the start of
//! the text so that nothing before the first token is lost.

use serde_json::{Map, Value};

/// How many pieces a field is cut into unless the configuration says
/// otherwise. A field cut into more than this is a field whose chunks are
/// unlikely to be worth embedding, and the reference stops rather than
/// filling an index with them.
const CHUNKS_AT_MOST: i64 = 100;

const TOKENIZERS: &[&str] =
    &["whitespace", "thai", "letter", "uax_url_email", "lowercase", "standard", "classic"];

/// The ways of cutting a text, in the order the reference lists them.
const ALGORITHMS: &[&str] = &["fixed_token_length", "delimiter", "fixed_char_length"];

pub(crate) enum Algorithm {
    FixedToken { token_limit: usize, overlap: usize, tokenizer: String, at_most: i64 },
    Delimiter { delimiter: String, at_most: i64 },
    FixedChar { char_limit: usize, overlap: usize, at_most: i64 },
}

/// A number written either as a number or as the text of one, which is how
/// the reference reads a processor's parameters.
fn number(cfg: &Map<String, Value>, key: &str) -> Option<f64> {
    match cfg.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn whole(cfg: &Map<String, Value>, key: &str, default: i64) -> i64 {
    number(cfg, key).map(|n| n as i64).unwrap_or(default)
}

/// How many units of a chunk the next chunk repeats: the rate is of the
/// chunk's size, and a fraction of a unit is no overlap at all.
fn overlap_of(cfg: &Map<String, Value>, limit: i64) -> Result<usize, String> {
    let rate = number(cfg, "overlap_rate").unwrap_or(0.0);
    if !(0.0..=0.5).contains(&rate) {
        return Err("Parameter [overlap_rate] must be between 0.0 and 0.5".into());
    }
    Ok((limit as f64 * rate).floor().max(0.0) as usize)
}

fn at_most_of(cfg: &Map<String, Value>) -> Result<i64, String> {
    let at_most = whole(cfg, "max_chunk_limit", CHUNKS_AT_MOST);
    if at_most <= 0 && at_most != -1 {
        return Err(
            "Parameter [max_chunk_limit] must be positive or -1 to disable this parameter".into()
        );
    }
    Ok(at_most)
}

impl Algorithm {
    /// Read the `algorithm` a `text_chunking` processor was given. The
    /// reference's own message is returned as it writes it, because the
    /// processor reports these as a failure of its own rather than as a
    /// property the request got wrong.
    pub(crate) fn parse(algorithm: &Map<String, Value>) -> Result<Algorithm, String> {
        if algorithm.len() > 1 {
            return Err(
                "Unable to create text_chunking processor as [algorithm] contains multiple \
                 algorithms"
                    .into(),
            );
        }
        let (name, cfg) = match algorithm.iter().next() {
            // no algorithm named is the first one with all of its defaults
            None => ("fixed_token_length", Map::new()),
            Some((name, Value::Object(cfg))) => (name.as_str(), cfg.clone()),
            Some((name, _)) => (name.as_str(), Map::new()),
        };
        if !ALGORITHMS.contains(&name) {
            return Err(format!(
                "Chunking algorithm [{name}] is not supported. Supported chunking algorithms are \
                 [{}]",
                ALGORITHMS.join(", ")
            ));
        }
        match name {
            "fixed_token_length" => {
                let token_limit = whole(&cfg, "token_limit", 384);
                if token_limit <= 0 {
                    return Err("Parameter [token_limit] must be positive.".into());
                }
                let tokenizer = match cfg.get("tokenizer") {
                    Some(Value::String(s)) => s.clone(),
                    _ => "standard".to_string(),
                };
                if !TOKENIZERS.contains(&tokenizer.as_str()) {
                    return Err(format!(
                        "Tokenizer [{tokenizer}] is not supported for [fixed_token_length] \
                         algorithm. Supported tokenizers are [{}]",
                        TOKENIZERS.join(", ")
                    ));
                }
                Ok(Algorithm::FixedToken {
                    token_limit: token_limit as usize,
                    overlap: overlap_of(&cfg, token_limit)?,
                    tokenizer,
                    at_most: at_most_of(&cfg)?,
                })
            }
            "fixed_char_length" => {
                let char_limit = whole(&cfg, "char_limit", 2048);
                if char_limit <= 0 {
                    return Err("Parameter [char_limit] must be positive.".into());
                }
                Ok(Algorithm::FixedChar {
                    char_limit: char_limit as usize,
                    overlap: overlap_of(&cfg, char_limit)?,
                    at_most: at_most_of(&cfg)?,
                })
            }
            _ => {
                let delimiter = match cfg.get("delimiter") {
                    Some(Value::String(s)) => s.clone(),
                    // a paragraph break, which is where prose divides
                    _ => "\n\n".to_string(),
                };
                if delimiter.is_empty() {
                    return Err("Parameter [delimiter] should not be empty.".into());
                }
                Ok(Algorithm::Delimiter { delimiter, at_most: at_most_of(&cfg)? })
            }
        }
    }

    /// The pieces a text is cut into. A text with nothing in it but space is
    /// not cut at all: there is nothing in it worth embedding.
    pub(crate) fn chunks(&self, text: &str) -> Vec<String> {
        if text.trim().is_empty() {
            return Vec::new();
        }
        match self {
            Algorithm::FixedToken { token_limit, overlap, tokenizer, at_most } => {
                by_tokens(text, *token_limit, *overlap, tokenizer, *at_most)
            }
            Algorithm::FixedChar { char_limit, overlap, at_most } => {
                by_chars(text, *char_limit, *overlap, *at_most)
            }
            Algorithm::Delimiter { delimiter, at_most } => by_delimiter(text, delimiter, *at_most),
        }
    }
}

/// Whether a chunk about to be cut is the last one allowed, so that what is
/// left of the text goes into it whole rather than being dropped.
fn last_allowed(so_far: usize, at_most: i64) -> bool {
    at_most > 0 && so_far as i64 == at_most - 1
}

fn by_tokens(
    text: &str,
    token_limit: usize,
    overlap: usize,
    tokenizer: &str,
    at_most: i64,
) -> Vec<String> {
    let chain = crate::analysis::Registry::from_settings(&Value::Null)
        .tokenizer_only(&Value::String(tokenizer.to_string()));
    let tokens = chain.cut(text);
    if tokens.is_empty() {
        return Vec::new();
    }
    // where each chunk begins again: the tokens the next chunk repeats are
    // taken off the step, and a limit of one token can never overlap
    let step = token_limit.saturating_sub(overlap).max(1);
    let mut out: Vec<String> = Vec::new();
    let mut first = 0usize;
    while first < tokens.len() {
        let past = first + token_limit;
        // the first chunk takes what stands before the first token with it
        let from = if out.is_empty() { 0 } else { tokens[first].2 };
        let stop = last_allowed(out.len(), at_most) && past < tokens.len();
        let to = if stop || past >= tokens.len() { text.len() } else { tokens[past].2 };
        out.push(text[from..to].to_string());
        if stop || past >= tokens.len() {
            break;
        }
        first += step;
    }
    out
}

fn by_chars(text: &str, char_limit: usize, overlap: usize, at_most: i64) -> Vec<String> {
    // Java counts a character as a UTF-16 unit, so the limit is in those
    // rather than in bytes or in Unicode scalars
    let mut at: Vec<usize> = Vec::new();
    for (byte, c) in text.char_indices() {
        for _ in 0..c.len_utf16() {
            at.push(byte);
        }
    }
    at.push(text.len());
    let units = at.len() - 1;
    let step = char_limit.saturating_sub(overlap).max(1);
    let mut out: Vec<String> = Vec::new();
    let mut first = 0usize;
    while first < units {
        let past = first + char_limit;
        let stop = last_allowed(out.len(), at_most) && past < units;
        let to = if stop || past >= units { units } else { past };
        out.push(text[at[first]..at[to]].to_string());
        if stop || past >= units {
            break;
        }
        first += step;
    }
    out
}

fn by_delimiter(text: &str, delimiter: &str, at_most: i64) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut from = 0usize;
    while let Some(found) = text[from..].find(delimiter) {
        if last_allowed(out.len(), at_most) {
            break;
        }
        let to = from + found + delimiter.len();
        out.push(text[from..to].to_string());
        from = to;
    }
    // the delimiter ends the piece before it, so a text ending in one has
    // nothing after it to keep
    if from < text.len() {
        out.push(text[from..].to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn cut(algorithm: Value, text: &str) -> Vec<String> {
        Algorithm::parse(algorithm.as_object().unwrap()).expect("algorithm").chunks(text)
    }

    fn refused(algorithm: Value) -> String {
        Algorithm::parse(algorithm.as_object().unwrap()).err().expect("refused")
    }

    #[test]
    fn a_chunk_keeps_the_space_after_its_last_word() {
        assert_eq!(
            cut(json!({"fixed_token_length": {"token_limit": 3}}), "one two three four five six"),
            ["one two three ", "four five six"]
        );
        // what stands before the first token belongs to the first chunk, and
        // what stands after the last to the last
        assert_eq!(
            cut(json!({"fixed_token_length": {"token_limit": 2}}), "   a b c d  "),
            ["   a b ", "c d  "]
        );
    }

    #[test]
    fn overlap_repeats_whole_tokens_only() {
        assert_eq!(
            cut(
                json!({"fixed_token_length": {"token_limit": 3, "overlap_rate": 0.5}}),
                "one two three four five six"
            ),
            ["one two three ", "three four five ", "five six"]
        );
        // a third of three tokens is no whole token, so nothing repeats
        assert_eq!(
            cut(
                json!({"fixed_token_length": {"token_limit": 3, "overlap_rate": 0.3}}),
                "one two three four five six"
            ),
            ["one two three ", "four five six"]
        );
    }

    #[test]
    fn the_last_chunk_allowed_holds_the_rest_of_the_text() {
        let text = "1 2 3 4 5 6 7 8 9 10 11 12";
        assert_eq!(
            cut(json!({"fixed_token_length": {"token_limit": 3, "max_chunk_limit": 2}}), text),
            ["1 2 3 ", "4 5 6 7 8 9 10 11 12"]
        );
        assert_eq!(
            cut(json!({"fixed_token_length": {"token_limit": 3, "max_chunk_limit": 1}}), text),
            [text]
        );
        assert_eq!(
            cut(json!({"delimiter": {"delimiter": ".", "max_chunk_limit": 2}}), "a.b.c.d"),
            ["a.", "b.c.d"]
        );
    }

    #[test]
    fn a_delimiter_ends_the_piece_it_stands_after() {
        assert_eq!(cut(json!({"delimiter": {"delimiter": "."}}), "a..b"), ["a.", ".", "b"]);
        assert_eq!(cut(json!({"delimiter": {"delimiter": "."}}), "a.b."), ["a.", "b."]);
        assert_eq!(cut(json!({"delimiter": {"delimiter": "|"}}), "abc"), ["abc"]);
        assert_eq!(cut(json!({"delimiter": {}}), "p1\n\np2"), ["p1\n\n", "p2"]);
    }

    #[test]
    fn characters_are_counted_where_tokens_are_not() {
        assert_eq!(
            cut(json!({"fixed_char_length": {"char_limit": 4}}), "abcdefghij"),
            ["abcd", "efgh", "ij"]
        );
        assert_eq!(
            cut(json!({"fixed_char_length": {"char_limit": 4, "overlap_rate": 0.5}}), "abcdefghij"),
            ["abcd", "cdef", "efgh", "ghij"]
        );
    }

    #[test]
    fn a_text_with_nothing_in_it_is_not_cut() {
        assert!(cut(json!({"fixed_token_length": {"token_limit": 2}}), "").is_empty());
        assert!(cut(json!({"fixed_token_length": {"token_limit": 2}}), "   ").is_empty());
        assert!(cut(json!({"delimiter": {"delimiter": "."}}), " ").is_empty());
    }

    #[test]
    fn what_cannot_be_cut_that_way_is_refused() {
        assert_eq!(
            refused(json!({"fixed_token_length": {"token_limit": 0}})),
            "Parameter [token_limit] must be positive."
        );
        assert_eq!(
            refused(json!({"fixed_token_length": {"overlap_rate": 0.6}})),
            "Parameter [overlap_rate] must be between 0.0 and 0.5"
        );
        assert_eq!(
            refused(json!({"fixed_token_length": {"max_chunk_limit": 0}})),
            "Parameter [max_chunk_limit] must be positive or -1 to disable this parameter"
        );
        assert_eq!(
            refused(json!({"nope": {}})),
            concat!(
                "Chunking algorithm [nope] is not supported. Supported chunking algorithms are ",
                "[fixed_token_length, delimiter, fixed_char_length]"
            )
        );
        assert_eq!(
            refused(json!({"delimiter": {}, "fixed_token_length": {}})),
            "Unable to create text_chunking processor as [algorithm] contains multiple algorithms"
        );
        assert_eq!(
            refused(json!({"delimiter": {"delimiter": ""}})),
            "Parameter [delimiter] should not be empty."
        );
        assert!(
            refused(json!({"fixed_token_length": {"tokenizer": "nope"}}))
                .starts_with("Tokenizer [nope] is not supported")
        );
        assert_eq!(
            refused(json!({"fixed_char_length": {"char_limit": -1}})),
            "Parameter [char_limit] must be positive."
        );
    }

    #[test]
    fn a_limit_disabled_cuts_as_far_as_the_text_goes() {
        let text = "1 2 3 4 5 6 7 8 9 10 11 12";
        assert_eq!(
            cut(json!({"fixed_token_length": {"token_limit": 3, "max_chunk_limit": -1}}), text)
                .len(),
            4
        );
    }
}
