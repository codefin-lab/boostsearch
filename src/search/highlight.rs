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
    // every path the mapping knows, plus whatever the document itself carries
    let mut candidates: Vec<String> = mapping.types.keys().cloned().collect();
    if let Some(o) = source.as_object() {
        for k in o.keys() {
            if !candidates.contains(k) {
                candidates.push(k.clone());
            }
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
        let text = source
            .pointer(&format!("/{}", name.replace('.', "/")))
            .and_then(|v| v.as_str())
            .or_else(|| {
                let (parent, _) = name.rsplit_once('.')?;
                source.pointer(&format!("/{}", parent.replace('.', "/")))?.as_str()
            });
        let Some(text) = text else { continue };

        // a value longer than `ignore_above` was never indexed, so there is
        // nothing in it that could have matched
        if let Some(limit) = mapping.field_option(&name, "ignore_above").and_then(|v| v.as_u64())
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
        let text = match opts.get("max_analyzer_offset").and_then(|v| v.as_u64()) {
            Some(cap) if plain => {
                let cap = (cap as usize).min(text.len());
                &text[..cap]
            }
            _ => text,
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
        let analyzer =
            mapping.field_option(&name, "analyzer").and_then(|v| v.as_str().map(|s| s.to_string()));
        let marked = mark_terms(index, text, &terms, analyzer.as_deref(), &pre, &post);
        if let Some(marked) = marked {
            out.insert(name, json!([marked]));
        }
    }
    (!out.is_empty()).then(|| Value::Object(out))
}

/// The text each field was searched for, gathered from the query.
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
                                    "prefix"
                                        | "wildcard"
                                        | "match_phrase_prefix"
                                        | "match_bool_prefix"
                                );
                                out.push((field.clone(), t, partial));
                            }
                        }
                    }
                }
                "multi_match" => {
                    let text = body.get("query").and_then(|v| v.as_str()).unwrap_or("");
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

/// Mark the tokens of `text` that the query's words match.
pub(crate) fn mark_terms(
    index: &boostcore::Index,
    text: &str,
    queries: &[(String, bool)],
    analyzer: Option<&str>,
    pre: &str,
    post: &str,
) -> Option<String> {
    let mut whole: std::collections::HashSet<String> = Default::default();
    let mut starts: Vec<String> = Vec::new();
    for (q, partial) in queries {
        for tok in crate::query::analyze_text(index, q, analyzer) {
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
        if whole.contains(&lower) || starts.iter().any(|p| lower.starts_with(p)) {
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
