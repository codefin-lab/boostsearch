//! Marking, in the text a document holds, the words a query asked for.

use super::*;

/// Wrap the terms a query looked for, wherever they appear in a hit's text.
///
/// The engine has no positional highlighter; what it does instead is analyse
/// the query's text the same way the field was analysed, then mark the tokens
/// of the stored value that match. That reproduces what a reader wants from a
/// highlight -- which words were found -- without a second index of offsets.
pub(crate) fn build_highlight(
    spec: &Value,
    source: &Value,
    query: &Option<Value>,
    mapping: &crate::store::Mapping,
    index: &boostcore::Index,
    analysis: &crate::analysis::Registry,
) -> Option<Value> {
    let fields = spec.get("fields")?;
    // where the document itself is not kept, only a field stored in its own
    // right has any text left to highlight
    let source_kept = mapping.raw.pointer("/_source/enabled") != Some(&json!(false));
    let patterns: Vec<(String, Value)> = match fields {
        Value::Object(o) => o.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        Value::Array(a) => a
            .iter()
            .filter_map(|f| f.as_object())
            .flat_map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>())
            .collect(),
        _ => return None,
    };
    let patterns: Vec<(String, Value)> = patterns
        .into_iter()
        .filter(|(name, _)| source_kept || mapping.field_option(name, "store") == Some(json!(true)))
        .collect();
    let tag = |key: &str, fallback: &str| -> String {
        spec.get(key)
            .and_then(|v| match v {
                Value::Array(a) => a.first().and_then(|x| x.as_str()).map(|s| s.to_string()),
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| fallback.to_string())
    };
    let pre = tag("pre_tags", "<em>");
    let post = tag("post_tags", "</em>");
    let require_match = spec.get("require_field_match").and_then(|v| v.as_bool()).unwrap_or(true);

    let asked = query_terms_by_field(query.as_ref());
    // a derived field's text is made from the source, not read out of it
    let derived_copy;
    let source = if mapping.derived_fields().is_empty() {
        source
    } else {
        derived_copy = crate::store::with_derived(source, mapping);
        &derived_copy
    };
    // every path the mapping knows, plus whatever the document itself carries
    let mut candidates: Vec<String> = mapping.types.keys().cloned().collect();
    if let Some(o) = source.as_object() {
        for k in o.keys() {
            if !candidates.contains(k) {
                candidates.push(k.clone());
            }
        }
    }
    // a field the request named by its full name is looked at even where the
    // mapping never wrote it down: the sub-fields a `search_as_you_type`
    // mapping makes are named that way
    for (pat, _) in &patterns {
        if !pat.contains('*') && !candidates.contains(pat) {
            candidates.push(pat.clone());
        }
    }
    candidates.sort();
    candidates.dedup();

    let mut out = serde_json::Map::new();
    for name in candidates {
        let wanted = patterns
            .iter()
            .any(|(pat, _)| pat == &name || pat == "*" || crate::store::glob_match(pat, &name));
        if !wanted {
            continue;
        }
        // the value lives at the field's own path, or at its parent's when the
        // field is a multi-field of another
        let held = source
            .pointer(&format!("/{}", name.replace('.', "/")))
            .filter(|v| v.is_string() || v.is_array())
            .or_else(|| {
                let (parent, _) = name.rsplit_once('.')?;
                source.pointer(&format!("/{}", parent.replace('.', "/")))
            });
        // a field holding several values is highlighted one value at a time,
        // and each that has something to mark is a fragment
        let texts: Vec<&str> = match held {
            Some(Value::String(s)) => vec![s.as_str()],
            Some(Value::Array(items)) => items.iter().filter_map(|v| v.as_str()).collect(),
            _ => continue,
        };
        let mut fragments: Vec<String> = Vec::new();
        for text in texts {
            // a value longer than `ignore_above` was never indexed, so there is
            // nothing in it that could have matched
            if let Some(limit) =
                mapping.field_option(&name, "ignore_above").and_then(|v| v.as_u64())
                && text.chars().count() as u64 > limit
            {
                continue;
            }
            // A per-field cap says how much of the value to analyse. The plain
            // highlighter returns what it analysed and nothing more; the unified
            // one still returns the whole field, having looked only that far for
            // something to mark.
            let opts = patterns
                .iter()
                .find(|(pat, _)| pat == &name || crate::store::glob_match(pat, &name))
                .map(|(_, o)| o.clone())
                .unwrap_or(Value::Null);
            let plain = opts.get("type").and_then(|t| t.as_str()) == Some("plain")
                || spec.get("type").and_then(|t| t.as_str()) == Some("plain");
            // `max_analyzer_offset` says how far into the field the analyser
            // is allowed to read: past it there are no tokens, so there is
            // nothing to mark. It is not the plain highlighter's alone.
            let (text, beyond) = match opts
                .get("max_analyzer_offset")
                .or_else(|| spec.get("max_analyzer_offset"))
                .and_then(|v| v.as_u64())
            {
                Some(cap) => {
                    let mut cap = (cap as usize).min(text.len());
                    while cap > 0 && !text.is_char_boundary(cap) {
                        cap -= 1;
                    }
                    // the analyser stops there. The plain highlighter builds
                    // its fragments out of what it analysed, so its answer
                    // stops there too; the unified one marks inside the whole
                    // field, and what lies beyond comes back as it stands.
                    (&text[..cap], if plain { "" } else { &text[cap..] })
                }
                None => (text, ""),
            };
            // a field may be highlighted against a query of its own rather than
            // against the one that found the document
            let own = opts
                .get("highlight_query")
                .or_else(|| spec.get("highlight_query"))
                .map(|q| query_terms_by_field(Some(q)));
            let terms = match own {
                Some(ref asked) => terms_for_field(asked, &name, require_match),
                None => terms_for_field(&asked, &name, require_match),
            };
            if terms.is_empty() {
                continue;
            }
            // the query's words are read the way a search reads them -- a stem
            // stacked on its word finds the word's other forms in the text
            let analyzer = ["search_analyzer", "analyzer"]
                .iter()
                .find_map(|key| mapping.field_option(&name, key))
                .and_then(|v| v.as_str().map(|s| s.to_string()));
            // how the text was cut is the index analyzer's doing, and decides
            // whether pieces or words are marked
            let indexed_with = mapping
                .field_option(&name, "analyzer")
                .and_then(|v| v.as_str().map(|s| s.to_string()));
            // a shingle sub-field holds runs of words rather than words, so what
            // is marked in the text is the run
            // a field cut into pieces of words matches on the pieces, so what is
            // marked is each piece wherever it stands inside a word
            let chain = indexed_with.as_deref().and_then(|named| analysis.get(named));
            let pieces = chain.as_ref().map(|c| c.cuts_into_ngrams()).unwrap_or(false);
            // pieces cut out of whole words keep the word's offsets, so a match
            // on a piece marks the word it came from
            let within_words = chain.as_ref().map(|c| c.filters_into_ngrams()).unwrap_or(false);
            // An annotated field carries its own markup -- `[shown](value)` --
            // and highlighting it means saying which annotations were hit
            // rather than wrapping words in tags: a reader of the answer gets
            // the same markup back with `_hit_term` added to what matched.
            let annotated = opts.get("type").and_then(|t| t.as_str()) == Some("annotated")
                || spec.get("type").and_then(|t| t.as_str()) == Some("annotated");
            if annotated {
                let hits: Vec<String> = terms.iter().map(|(t, _)| t.clone()).collect();
                let readers = vec![analyzer.clone()];
                if let Some(marked) = mark_annotated(index, text, &terms, &hits, &readers, analysis)
                {
                    fragments.push(marked);
                }
                continue;
            }
            let marked = match shingle_width(&name) {
                Some(width) => mark_runs(text, &terms, width, &pre, &post),
                None if pieces => mark_pieces(text, &terms, &pre, &post),
                None if within_words => mark_words_containing(text, &terms, &pre, &post),
                None => {
                    // the fields a highlight is told to match through lend their
                    // analyzers: a stop word the field drops is still marked when
                    // a plain copy of the field kept it
                    let mut readers: Vec<Option<String>> = vec![analyzer.clone()];
                    for other in opts
                        .get("matched_fields")
                        .and_then(|m| m.as_array())
                        .into_iter()
                        .flatten()
                        .filter_map(|v| v.as_str())
                    {
                        let named = ["search_analyzer", "analyzer"]
                            .iter()
                            .find_map(|key| mapping.field_option(other, key))
                            .and_then(|v| v.as_str().map(|s| s.to_string()));
                        readers.push(named);
                    }
                    mark_terms(index, text, &terms, &readers, analysis, &pre, &post)
                }
            };
            if let Some(mut marked) = marked {
                marked.push_str(beyond);
                fragments.push(marked);
            }
        }
        if !fragments.is_empty() {
            out.insert(name, json!(fragments));
        }
    }
    (!out.is_empty()).then(|| Value::Object(out))
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

/// How many words a token of this field holds, where the field is one of the
/// shingle sub-fields a `search_as_you_type` mapping makes.
fn shingle_width(field: &str) -> Option<usize> {
    let (_, leaf) = field.rsplit_once('.')?;
    let n = leaf.strip_prefix('_')?.strip_suffix("gram")?;
    n.parse::<usize>().ok().filter(|w| *w > 1)
}

/// Mark the runs of `width` words that the query's runs match.
///
/// The whole run is marked once rather than word by word: a shingle is one
/// token, and what stands for it in the text is the words it was made of.
fn mark_runs(
    text: &str,
    queries: &[(String, bool)],
    width: usize,
    pre: &str,
    post: &str,
) -> Option<String> {
    // every run of `width` words the query asked for
    let mut wanted: std::collections::HashSet<Vec<String>> = Default::default();
    for (q, _) in queries {
        let words: Vec<String> = q.split_whitespace().map(|w| w.to_lowercase()).collect();
        for run in words.windows(width) {
            wanted.insert(run.to_vec());
        }
    }
    if wanted.is_empty() {
        return None;
    }
    // the words of the text, with where each of them stands in it
    let mut words: Vec<(usize, usize, String)> = Vec::new();
    let mut at = 0usize;
    while at < text.len() {
        let rest = &text[at..];
        let Some(start) = rest.find(|c: char| c.is_alphanumeric()) else { break };
        let word = &rest[start..];
        let end = word.find(|c: char| !c.is_alphanumeric() && c != '_').unwrap_or(word.len());
        words.push((at + start, at + start + end, word[..end].to_lowercase()));
        at += start + end;
    }
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for (i, run) in words.windows(width).enumerate() {
        let here: Vec<String> = run.iter().map(|(_, _, w)| w.clone()).collect();
        if wanted.contains(&here) {
            spans.push((words[i].0, run[width - 1].1));
        }
    }
    if spans.is_empty() {
        return None;
    }
    // runs that touch are marked as one
    spans.sort();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (from, to) in spans {
        match merged.last_mut() {
            Some((_, before)) if *before >= from => *before = (*before).max(to),
            _ => merged.push((from, to)),
        }
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut cursor = 0usize;
    for (from, to) in merged {
        out.push_str(&text[cursor..from]);
        out.push_str(pre);
        out.push_str(&text[from..to]);
        out.push_str(post);
        cursor = to;
    }
    out.push_str(&text[cursor..]);
    Some(out)
}

/// Mark every word of the text that holds a query word inside it.
fn mark_words_containing(
    text: &str,
    queries: &[(String, bool)],
    pre: &str,
    post: &str,
) -> Option<String> {
    let wanted: Vec<String> = queries
        .iter()
        .flat_map(|(q, _)| q.split_whitespace().map(|w| w.to_lowercase()))
        .filter(|w| !w.is_empty())
        .collect();
    if wanted.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut marked = false;
    let mut rest = text;
    while !rest.is_empty() {
        let Some(start) = rest.find(|c: char| c.is_alphanumeric()) else { break };
        out.push_str(&rest[..start]);
        let word = &rest[start..];
        let end = word.find(|c: char| !c.is_alphanumeric() && c != '_').unwrap_or(word.len());
        let (word, tail) = word.split_at(end);
        let lower = word.to_lowercase();
        if wanted.iter().any(|w| lower.contains(w.as_str())) {
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

/// Mark every place a query word stands inside the text, whole word or not.
fn mark_pieces(text: &str, queries: &[(String, bool)], pre: &str, post: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for (q, _) in queries {
        for word in q.split_whitespace() {
            let word = word.to_lowercase();
            if word.is_empty() {
                continue;
            }
            let mut from = 0;
            while let Some(at) = lower[from..].find(&word) {
                let start = from + at;
                spans.push((start, start + word.len()));
                from = start + word.len();
            }
        }
    }
    if spans.is_empty() {
        return None;
    }
    spans.sort();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (from, to) in spans {
        match merged.last_mut() {
            Some((_, before)) if *before >= from => *before = (*before).max(to),
            _ => merged.push((from, to)),
        }
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut cursor = 0usize;
    for (from, to) in merged {
        if !text.is_char_boundary(from) || !text.is_char_boundary(to) {
            continue;
        }
        out.push_str(&text[cursor..from]);
        out.push_str(pre);
        out.push_str(&text[from..to]);
        out.push_str(post);
        cursor = to;
    }
    out.push_str(&text[cursor..]);
    Some(out)
}

/// Mark the tokens of `text` that the query's words match.
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
    index: &boostcore::Index,
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
    index: &boostcore::Index,
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
