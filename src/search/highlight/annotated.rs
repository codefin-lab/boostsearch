//! The annotated highlighter, and the query's words read the simple way it
//! reads them.
//!
//! An annotated field carries its own markup -- `[shown](value)` -- and
//! highlighting it means saying which annotations were hit rather than
//! wrapping words in tags: a reader of the answer gets the same markup back
//! with `_hit_term` added to what matched, one value at a time.

use serde_json::Value;

/// Each value of an annotated field, with its hits named in its markup.
pub(super) fn highlight(
    values: &[String],
    name: &str,
    opts: &super::Opts,
    query: &Option<Value>,
    mapping: &crate::store::Mapping,
    index: &velocore::Index,
    analysis: &crate::analysis::Registry,
) -> Vec<String> {
    let asked = match &opts.highlight_query {
        Some(q) => query_terms_by_field(Some(q)),
        None => query_terms_by_field(query.as_ref()),
    };
    let terms = terms_for_field(&asked, name, opts.require_field_match);
    if terms.is_empty() {
        return Vec::new();
    }
    // the query's words are read the way a search reads them
    let analyzer = ["search_analyzer", "analyzer"]
        .iter()
        .find_map(|key| mapping.field_option(name, key))
        .and_then(|v| v.as_str().map(|s| s.to_string()));
    let hits: Vec<String> = terms.iter().map(|(t, _)| t.clone()).collect();
    let readers = vec![analyzer];
    values
        .iter()
        .filter_map(|text| mark_annotated(index, text, &terms, &hits, &readers, analysis))
        .collect()
}

/// The text each field was searched for, gathered from the query.
/// The words of a bool prefix query that are whole words.
///
/// The last one is the beginning of a word, which a `search_as_you_type`
/// field answers from the terms it keeps of word beginnings rather than from
/// its own; nothing in this field's text stands for it.
fn whole_words(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    match words.len() {
        0 | 1 => String::new(),
        n => words[..n - 1].join(" "),
    }
}

pub(crate) fn query_terms_by_field(query: Option<&Value>) -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    fn walk(node: &Value, out: &mut Vec<(String, String, bool)>) {
        let Some(o) = node.as_object() else {
            if let Value::Array(a) = node {
                a.iter().for_each(|v| walk(v, out));
            }
            return;
        };
        for (kind, body) in o {
            match kind.as_str() {
                "match"
                | "match_phrase"
                | "match_phrase_prefix"
                | "term"
                | "prefix"
                | "wildcard"
                | "match_bool_prefix" => {
                    if let Some(inner) = body.as_object() {
                        for (field, spec) in inner {
                            let text = match spec {
                                Value::String(s) => Some(s.clone()),
                                Value::Object(so) => so
                                    .get("value")
                                    .or_else(|| so.get("query"))
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string()),
                                other => other.as_f64().map(|n| n.to_string()),
                            };
                            if let Some(t) = text {
                                // a prefix or wildcard names the start of a
                                // word rather than the whole of it
                                let partial = matches!(
                                    kind.as_str(),
                                    "prefix" | "wildcard" | "match_phrase_prefix"
                                );
                                // the last word of a bool prefix query is
                                // answered by the field of word beginnings,
                                // not by this one, so it marks nothing here
                                let t = match kind.as_str() {
                                    "match_bool_prefix" => whole_words(&t),
                                    _ => t,
                                };
                                if !t.is_empty() {
                                    out.push((field.clone(), t, partial));
                                }
                            }
                        }
                    }
                }
                "multi_match" => {
                    let asked = body.get("query").and_then(|v| v.as_str()).unwrap_or("");
                    let trimmed = match body.get("type").and_then(|v| v.as_str()) {
                        Some("bool_prefix") => whole_words(asked),
                        _ => asked.to_string(),
                    };
                    let text = trimmed.as_str();
                    if text.is_empty() {
                        continue;
                    }
                    let fields = body.get("fields").and_then(|f| f.as_array());
                    match fields {
                        Some(fs) => {
                            for f in fs.iter().filter_map(|f| f.as_str()) {
                                out.push((
                                    f.split('^').next().unwrap_or(f).to_string(),
                                    text.to_string(),
                                    false,
                                ));
                            }
                        }
                        None => out.push(("*".to_string(), text.to_string(), false)),
                    }
                }
                "query_string" | "simple_query_string" => {
                    let text = body.get("query").and_then(|v| v.as_str()).unwrap_or("");
                    let field = body.get("default_field").and_then(|v| v.as_str()).unwrap_or("*");
                    out.push((field.to_string(), text.to_string(), false));
                }
                _ => walk(body, out),
            }
        }
    }
    if let Some(q) = query {
        walk(q, &mut out);
    }
    out
}

/// Which of the query's texts apply to this field.
pub(crate) fn terms_for_field(
    asked: &[(String, String, bool)],
    field: &str,
    require_match: bool,
) -> Vec<(String, bool)> {
    asked
        .iter()
        .filter(|(pat, _, _)| {
            if !require_match {
                return true;
            }
            pat == field
                || pat == "*"
                || crate::store::glob_match(pat, field)
                // `text*` names `text` and its multi-fields alike
                || field.starts_with(&format!("{pat}."))
        })
        .map(|(_, text, partial)| (text.clone(), *partial))
        .collect()
}
/// One `[shown](value)` in an annotated field, by where its text stands once
/// the markup is taken off.
pub(crate) struct Annotation {
    pub(crate) from: usize,
    pub(crate) to: usize,
    /// what the annotation says, as written -- several values are joined
    /// with `&`, each one a thing the span is said to be
    pub(crate) raw: String,
}

/// An annotated field as the text somebody wrote and the annotations on it.
pub(crate) fn without_markup(text: &str) -> (String, Vec<Annotation>) {
    let mut plain = String::with_capacity(text.len());
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        // `[shown](value)` and nothing else: a bracket with no annotation
        // after it is a bracket somebody wrote
        let after = &rest[open + 1..];
        let shape = after
            .find(']')
            .filter(|close| after[close + 1..].starts_with('('))
            .and_then(|close| after[close + 2..].find(')').map(|end| (close, end)));
        let Some((close, end)) = shape else {
            plain.push_str(&rest[..open + 1]);
            rest = after;
            continue;
        };
        plain.push_str(&rest[..open]);
        let from = plain.len();
        plain.push_str(&after[..close]);
        found.push(Annotation {
            from,
            to: plain.len(),
            raw: after[close + 2..close + 2 + end].to_string(),
        });
        rest = &after[close + 2 + end + 1..];
    }
    plain.push_str(rest);
    (plain, found)
}

/// An annotated field with `_hit_term` added to what the query found.
///
/// Two things can be hit: an annotation, when the query asked for what it
/// says, and a word of the text itself. An annotation that was hit comes back
/// with the hit named in front of what it already said; a word that was hit
/// becomes an annotation of its own. Everything else comes back as plain
/// text, markup and all -- an annotation nobody asked about is not part of
/// the answer to this query.
fn mark_annotated(
    index: &velocore::Index,
    text: &str,
    queries: &[(String, bool)],
    hits: &[String],
    analyzers: &[Option<String>],
    analysis: &crate::analysis::Registry,
) -> Option<String> {
    const OPEN: &str = "\u{1}";
    const CLOSE: &str = "\u{2}";
    let (plain, annotations) = without_markup(text);
    // the words of the text that were hit, found the way any highlight finds
    // them, and then read back off the marked copy
    let marked = mark_terms(index, &plain, queries, analyzers, analysis, OPEN, CLOSE);
    let mut spans: Vec<(usize, usize)> = Vec::new();
    if let Some(marked) = &marked {
        let mut at = 0;
        let mut plain_at = 0;
        while let Some(open) = marked[at..].find(OPEN) {
            plain_at += marked[at..at + open].chars().count();
            let from = plain_at;
            let rest = at + open + OPEN.len();
            let Some(close) = marked[rest..].find(CLOSE) else { break };
            plain_at += marked[rest..rest + close].chars().count();
            spans.push((from, plain_at));
            at = rest + close + CLOSE.len();
        }
    }
    // the spans are in characters and the annotations in bytes; one map
    // between them, made once
    let byte_of: Vec<usize> =
        plain.char_indices().map(|(at, _)| at).chain(std::iter::once(plain.len())).collect();
    let spans: Vec<(usize, usize)> =
        spans.into_iter().filter_map(|(a, b)| Some((*byte_of.get(a)?, *byte_of.get(b)?))).collect();
    let hit_of = |annotation: &Annotation| -> Option<String> {
        annotation.raw.split('&').find(|value| hits.iter().any(|h| h == value)).map(str::to_string)
    };
    if spans.is_empty() && !annotations.iter().any(|a| hit_of(a).is_some()) {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0usize;
    while at < plain.len() {
        if let Some(a) = annotations.iter().find(|a| a.from == at)
            && let Some(hit) = hit_of(a)
        {
            out.push_str(&format!("[{}](_hit_term={hit}&{})", &plain[a.from..a.to], a.raw));
            at = a.to;
            continue;
        }
        if let Some((from, to)) = spans.iter().find(|(from, _)| *from == at) {
            out.push_str(&format!("[{0}](_hit_term={0})", &plain[*from..*to]));
            at = *to;
            continue;
        }
        let next = plain[at..].chars().next()?;
        out.push(next);
        at += next.len_utf8();
    }
    Some(out)
}

pub(crate) fn mark_terms(
    index: &velocore::Index,
    text: &str,
    queries: &[(String, bool)],
    analyzers: &[Option<String>],
    analysis: &crate::analysis::Registry,
    pre: &str,
    post: &str,
) -> Option<String> {
    let mut whole: std::collections::HashSet<String> = Default::default();
    let mut starts: Vec<String> = Vec::new();
    // the chain itself reads the query, so that every form it stacks in a
    // place -- a stem beside its word -- is a form to mark
    for (q, partial) in queries {
        let mut forms: Vec<String> = Vec::new();
        for analyzer in analyzers {
            forms.extend(match analyzer.as_deref().and_then(|named| analysis.get(named)) {
                Some(chain) => chain.terms(q),
                None => crate::query::analyze_text(index, q, analyzer.as_deref()),
            });
        }
        for tok in forms {
            if *partial {
                starts.push(tok);
            } else {
                whole.insert(tok);
            }
        }
    }
    if whole.is_empty() && starts.is_empty() {
        return None;
    }
    // walk the words of the original text, so punctuation and spacing survive
    let mut out = String::with_capacity(text.len() + 16);
    let mut marked = false;
    let mut rest = text;
    // a word in the text is read the same way the query was: an analyzer that
    // stems -- or folds, or maps -- makes a token the plain word never equals,
    // and the word it came from is what a highlight marks
    let mut forms_of: std::collections::HashMap<String, Vec<String>> = Default::default();
    while !rest.is_empty() {
        let start = match rest.find(|c: char| c.is_alphanumeric()) {
            Some(i) => i,
            None => break,
        };
        out.push_str(&rest[..start]);
        let word = &rest[start..];
        let end = word.find(|c: char| !c.is_alphanumeric() && c != '_').unwrap_or(word.len());
        let (word, tail) = word.split_at(end);
        let lower = word.to_lowercase();
        let hit = whole.contains(&lower) || starts.iter().any(|p| lower.starts_with(p)) || {
            let forms = forms_of.entry(lower.clone()).or_insert_with(|| {
                let mut f: Vec<String> = Vec::new();
                for analyzer in analyzers {
                    f.extend(match analyzer.as_deref().and_then(|named| analysis.get(named)) {
                        Some(chain) => chain.terms(&lower),
                        None => crate::query::analyze_text(index, &lower, analyzer.as_deref()),
                    });
                }
                f
            });
            forms.iter().any(|t| whole.contains(t) || starts.iter().any(|p| t.starts_with(p)))
        };
        if hit {
            out.push_str(pre);
            out.push_str(word);
            out.push_str(post);
            marked = true;
        } else {
            out.push_str(word);
        }
        rest = tail;
    }
    out.push_str(rest);
    marked.then_some(out)
}
