//! Asking what a query means, and what an analyzer does to a text.

use super::*;
use crate::analysis::Token;

/// A filter written the short way, spelled out.
///
/// `{"term": {"field": "value"}}` and `{"term": {"field": {"value": "value"}}}`
/// mean the same thing; the long form is what a filter is reported as, since
/// it is where the boost would go.
pub(crate) fn expand_filter(f: &Value) -> Value {
    let Some(o) = f.as_object() else { return f.clone() };
    let mut out = serde_json::Map::new();
    for (kind, body) in o {
        if !matches!(kind.as_str(), "term" | "prefix" | "wildcard" | "regexp" | "fuzzy") {
            out.insert(kind.clone(), body.clone());
            continue;
        }
        let Some(fields) = body.as_object() else {
            out.insert(kind.clone(), body.clone());
            continue;
        };
        let mut spelled = serde_json::Map::new();
        for (field, v) in fields {
            let long = match v {
                Value::Object(_) => v.clone(),
                other => json!({"value": other, "boost": 1.0}),
            };
            spelled.insert(field.clone(), long);
        }
        out.insert(kind.clone(), Value::Object(spelled));
    }
    Value::Object(out)
}

pub async fn validate_query(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let shards = json!({"total": 1, "successful": 1, "failed": 0});
    // a body that is not empty and does not name a query is not a query at
    // all, whatever else it contains
    let Some(query) = body.get("query").cloned() else {
        if body.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            let mut out = json!({"_shards": shards, "valid": true});
            if p.get("explain").map(|v| v != "false").unwrap_or(false) {
                let sample = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
                out["explanations"] = json!(
                    sample
                        .iter()
                        .map(|n| json!({
                            "index": n, "valid": true,
                            "explanation": describe_query(&json!({"match_all": {}})),
                        }))
                        .collect::<Vec<_>>()
                );
            }
            return respond(&p, out);
        }
        let mut out = json!({"_shards": shards, "valid": false});
        // whatever the body holds, it is not where a query goes -- said only
        // when the caller asked to be told why
        if p.get("explain").map(|v| v != "false").unwrap_or(false)
            && let Some(first) = body.as_object().and_then(|o| o.keys().next())
        {
            out["error"] = json!(format!("request does not support [{first}]"));
        }
        return respond(&p, out);
    };
    let probe = json!({"query": query, "size": 0});
    // building the query against one of the targets says whether it can be
    // read at all, and `explain` asks to be told why not
    let sample = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
    if let Some(st) = sample.first().and_then(|n| store.get(n)) {
        let g = st.read();
        let ctx = crate::query::Ctx {
            fields: &g.fields,
            mapping: &g.mapping,
            analysis: &g.analysis,
            index: &g.index,
            max_terms_count: g.max_terms_count(),
            max_regex_length: g.max_regex_length(),
            allow_expensive: crate::search::expensive_allowed(&store),
            observed_kinds: &g.observed_kinds,
            kinds_complete: g.kinds_complete,
            stats: &g.stats,
        };
        if let Err(e) = crate::query::build(&ctx, &query) {
            let mut out = json!({"_shards": shards, "valid": false});
            if p.get("explain").map(|v| v != "false").unwrap_or(false) {
                // the name of the index it was read against, then what went
                // wrong, which is the shape the message has
                let name = sample.first().cloned().unwrap_or_default();
                out["error"] = json!(format!("[{name}] QueryShardException[{e}]"));
            }
            return respond(&p, out);
        }
    }
    match crate::search::run(&store, &expr, &probe, &Params::new()) {
        Ok(_) => {
            let mut out = json!({"_shards": shards, "valid": true});
            if p.get("explain").map(|v| v != "false").unwrap_or(false) {
                out["explanations"] = json!(
                    sample
                        .iter()
                        .map(|n| json!({
                            "index": n, "valid": true,
                            "explanation": describe_query(&query),
                        }))
                        .collect::<Vec<_>>()
                );
            }
            respond(&p, out)
        }
        Err(_) => respond(&p, json!({"_shards": shards, "valid": false})),
    }
}

/// How a query reads once it has been rewritten, in the shape the engine
/// names its own queries.
pub(crate) fn describe_query(q: &Value) -> String {
    let Some((kind, body)) = q.as_object().and_then(|o| o.iter().next()) else {
        return "*:*".to_string();
    };
    match kind.as_str() {
        "match_all" => {
            "ApproximateScoreQuery(originalQuery=*:*, approximationQuery=Approximate(*:*))"
                .to_string()
        }
        "match_phrase" | "match_phrase_prefix" => body
            .as_object()
            .and_then(|o| o.iter().next())
            .map(|(f, v)| {
                let text = match v {
                    Value::String(s) => s.clone(),
                    Value::Object(o) => o
                        .get("query")
                        .map(|x| x.as_str().unwrap_or_default().to_string())
                        .unwrap_or_default(),
                    other => other.to_string(),
                };
                let words: Vec<&str> = text.split_whitespace().collect();
                match (kind.as_str(), words.len()) {
                    ("match_phrase_prefix", _) => format!("{f}:\"{}*\"", words.join(" ")),
                    (_, 1) => format!("{f}:{}", words[0]),
                    _ => format!("{f}:\"{}\"", words.join(" ")),
                }
            })
            .unwrap_or_else(|| "*:*".to_string()),
        "term" | "match" => body
            .as_object()
            .and_then(|o| o.iter().next())
            .map(|(f, v)| {
                let text = match v {
                    Value::String(s) => s.clone(),
                    Value::Object(o) => o
                        .get("value")
                        .or_else(|| o.get("query"))
                        .map(|x| x.as_str().unwrap_or_default().to_string())
                        .unwrap_or_default(),
                    other => other.to_string(),
                };
                format!("{f}:{text}")
            })
            .unwrap_or_else(|| "*:*".to_string()),
        other => other.to_string(),
    }
}

/// `_analyze` runs text through the tokenizer the query path would use.
/// How many times a term counts, when the text said so: `foo^3` is three.
fn frequency_of(token: &str) -> u64 {
    token.rsplit_once('^').and_then(|(_, n)| n.parse::<u64>().ok()).unwrap_or(1)
}

pub async fn analyze(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let text = match body.get("text") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => {
            a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()
        }
        _ => p.get("text").map(|t| vec![t.clone()]).unwrap_or_default(),
    };
    // a normalizer does not cut its text, so a filter that splits a token has
    // no place in one: asked for without a tokenizer, those are refused
    if body.get("tokenizer").is_none()
        && !p.contains_key("tokenizer")
        && let Some(filters) = body.get("filter").and_then(|f| f.as_array())
    {
        const SPLITS: &[&str] = &[
            "word_delimiter",
            "word_delimiter_graph",
            "ngram",
            "edge_ngram",
            "shingle",
            "synonym_graph",
        ];
        for named in filters {
            let kind = match named {
                Value::String(s) => s.as_str(),
                other => other.get("type").and_then(|t| t.as_str()).unwrap_or(""),
            };
            if SPLITS.contains(&kind) {
                return err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!("Custom normalizer may not use filter [{kind}]"),
                );
            }
        }
    }
    // an ngram tokenizer that spans more widths than the index allows makes
    // more tokens than anyone asked for
    if let Some(spec) = body.get("tokenizer").filter(|t| t.is_object()) {
        let kind = spec.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let read =
            |key: &str, fallback: u64| spec.get(key).and_then(|v| v.as_u64()).unwrap_or(fallback);
        if kind == "ngram" {
            let span = read("max_gram", 2).saturating_sub(read("min_gram", 1));
            if span > 1 {
                return err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!(
                        "The difference between max_gram and min_gram in NGram Tokenizer must \
                         be less than or equal to: [1] but was [{span}]. This limit can be set \
                         by changing the [index.max_ngram_diff] index level setting."
                    ),
                );
            }
        }
    }
    let analyzer = body
        .get("analyzer")
        .and_then(|v| v.as_str())
        .or_else(|| p.get("analyzer").map(|s| s.as_str()));
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let st = store.resolve(&expr).into_iter().next().and_then(|n| store.get(&n));
    // a tokenizer only splits; folding case is a filter, and naming one
    // without the other asks for the split alone
    let tokenizer_only =
        analyzer.is_none() && (body.get("tokenizer").is_some() || p.contains_key("tokenizer"));
    // What the request asked for, as a chain: a name, a field's own analyzer,
    // or the parts spelled out. An index that defined any of them is the one
    // asked, so this is read through its registry.
    // an index that defined analyzers of its own answers through its registry;
    // asked without one, the built-ins still are what OpenSearch has
    let registry = match &st {
        Some(s) => s.read().analysis.clone(),
        None => crate::analysis::Registry::default(),
    };
    let chain = {
        let g = st.as_ref().map(|s| s.read());
        let g = g.as_deref();
        if let Some(name) = analyzer {
            registry.get(name)
        } else if let Some(field) = body.get("field").and_then(|v| v.as_str()) {
            g.and_then(|g| {
                ["search_analyzer", "analyzer"]
                    .iter()
                    .find_map(|key| g.mapping.field_option(field, key))
                    .and_then(|v| v.as_str().map(|n| n.to_string()))
                    .and_then(|name| registry.get(&name))
            })
        } else if let Some(name) = body.get("normalizer").and_then(|v| v.as_str()) {
            registry.get(name)
        } else if body.get("tokenizer").is_some()
            || body.get("filter").is_some()
            || body.get("char_filter").is_some()
            || p.contains_key("tokenizer")
        {
            let named = body
                .get("tokenizer")
                .cloned()
                .or_else(|| p.get("tokenizer").map(|t| json!(t)))
                .unwrap_or_else(|| json!("standard"));
            let filters = body.get("filter").cloned().unwrap_or_else(|| json!([]));
            let chars = body.get("char_filter").cloned().unwrap_or_else(|| json!([]));
            registry.custom(&json!({"tokenizer": named, "filter": filters, "char_filter": chars}))
        } else {
            registry.get("standard")
        }
    };
    let mut tokens = Vec::new();
    let mut pos = 0usize;
    for t in &text {
        // where a token came from is part of the answer: a highlighter and a
        // caller reading `_analyze` both ask for it
        let parts: Vec<Token> = if let Some(chain) = &chain {
            chain.tokens(t)
        } else if tokenizer_only {
            t.split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
                .enumerate()
                .map(|(i, w)| (w.to_string(), i, 0, 0, 1))
                .collect()
        } else {
            match &st {
                Some(s) => crate::query::analyze_text(&s.read().index, t, analyzer)
                    .into_iter()
                    .enumerate()
                    .map(|(i, w)| (w, i, 0, 0, 1))
                    .collect(),
                None => t
                    .split_whitespace()
                    .enumerate()
                    .map(|(i, w)| (w.to_lowercase(), i, 0, 0, 1))
                    .collect(),
            }
        };
        let parts_len = parts.iter().map(|(_, at, _, _, _)| *at + 1).max().unwrap_or(0);
        for (tok, at, from, to, length) in parts {
            tokens.push(json!({
                "token": tok, "start_offset": from, "end_offset": to,
                "type": "<ALPHANUM>", "position": pos + at,
            }));
            // a token standing for more than one word says so
            if length > 1
                && let Some(last) = tokens.last_mut()
            {
                last["positionLength"] = json!(length);
            }
        }
        pos += parts_len;
    }
    let cap = st
        .as_ref()
        .and_then(|s| s.read().numeric_setting("analyze.max_token_count"))
        .unwrap_or(10_000) as usize;
    if tokens.len() > cap {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "The number of tokens produced by calling _analyze has exceeded the allowed \
                 maximum of [{cap}]. This limit can be set by changing the \
                 [index.analyze.max_token_count] index level setting."
            ),
        );
    }
    // `explain` asks for the tokens as each step left them, rather than as
    // one flat list: the tokenizer first, then one entry per filter over it
    if body.get("explain").and_then(|v| v.as_bool()).unwrap_or(false)
        || p.get("explain").map(|v| v == "true").unwrap_or(false)
    {
        let as_json = |cut: Vec<Token>| -> Vec<Value> {
            cut.into_iter()
                .map(|(token, at, from, to, _)| {
                    json!({
                        "token": token, "start_offset": from, "end_offset": to,
                        "type": "<ALPHANUM>", "position": at,
                        "bytes": format!("[{}]", token.as_bytes().iter().map(|b| format!("{b:x}"))
                            .collect::<Vec<_>>().join(" ")),
                        "positionLength": 1,
                        "termFrequency": frequency_of(&token),
                        // whether a filter held this word back from the
                        // stemmers; nothing marks one at this point
                        "keyword": false,
                    })
                })
                .collect()
        };
        // filters asked for on their own stand on the text whole: that is a
        // normalizer, and a normalizer's tokenizer is `keyword`
        let named_tokenizer = body
            .get("tokenizer")
            .cloned()
            .or_else(|| p.get("tokenizer").map(|t| json!(t)))
            .or_else(|| {
                (analyzer.is_none()
                    && (body.get("filter").is_some() || body.get("char_filter").is_some()))
                .then(|| json!("keyword"))
            });
        if let Some(spec) = named_tokenizer {
            // a tokenizer named as a string is reported under that name; one
            // described in the request has no name of its own, and is
            // reported under the type it gave
            let name = match &spec {
                Value::String(s) => s.clone(),
                other => format!(
                    "__anonymous__{}",
                    other.get("type").and_then(|t| t.as_str()).unwrap_or("tokenizer")
                ),
            };
            // what the char filters made of the text, before it was cut
            let asked_chars: Vec<Value> =
                body.get("char_filter").and_then(|c| c.as_array()).cloned().unwrap_or_default();
            let prepared: Vec<String> = {
                let filters = registry.char_filters(&asked_chars);
                text.iter()
                    .map(|t| filters.iter().fold(t.clone(), |held, filter| filter.applied(&held)))
                    .collect()
            };
            let charfilters: Vec<Value> = asked_chars
                .iter()
                .map(|one| {
                    let name = match one {
                        Value::String(s) => s.clone(),
                        other => format!(
                            "__anonymous__{}",
                            other.get("type").and_then(|t| t.as_str()).unwrap_or("char_filter")
                        ),
                    };
                    json!({"name": name, "filtered_text": prepared.clone()})
                })
                .collect();
            let base = registry.tokenizer_only(&spec);
            let cut: Vec<Token> = prepared.iter().flat_map(|t| base.cut(t)).collect();
            let stage = json!({"name": name, "tokens": as_json(cut)});
            // each filter is reported as the tokens standing after it, so the
            // chain is run again one filter longer each time
            let asked: Vec<Value> =
                body.get("filter").and_then(|f| f.as_array()).cloned().unwrap_or_default();
            let mut steps = Vec::new();
            let mut filters = Vec::new();
            // the tokens as the stage before left them: a filter that reads a
            // frequency off the end of a word reports the frequency it read
            let mut before: Vec<Token> = prepared.iter().flat_map(|t| base.cut(t)).collect();
            for one in &asked {
                steps.extend(registry.filter_steps(one));
                let name = match one {
                    Value::String(s) => s.clone(),
                    other => format!(
                        "__anonymous__{}",
                        other.get("type").and_then(|t| t.as_str()).unwrap_or("filter")
                    ),
                };
                let chain =
                    crate::analysis::Chain::of(registry.tokenizer_only(&spec), steps.clone());
                let cut: Vec<Token> = prepared.iter().flat_map(|t| chain.tokens(t)).collect();
                let mut listed = as_json(cut.clone());
                if steps.iter().any(|s| matches!(s, crate::analysis::Step::DelimitedTermFreq(_))) {
                    for (at, token) in listed.iter_mut().enumerate() {
                        if let Some((was, _, _, _, _)) = before.get(at) {
                            token["termFrequency"] = json!(frequency_of(was));
                        }
                    }
                }
                before = cut;
                filters.push(json!({"name": name, "tokens": listed}));
            }
            let mut detail = json!({
                "custom_analyzer": true,
                "tokenizer": stage,
                "tokenfilters": filters,
            });
            if !charfilters.is_empty() {
                detail["charfilters"] = json!(charfilters);
            }
            return respond(&p, json!({ "detail": detail }));
        }
        let name = body
            .get("analyzer")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .or_else(|| p.get("analyzer").cloned())
            .or_else(|| body.get("field").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .unwrap_or_else(|| "standard".to_string());
        return respond(
            &p,
            json!({"detail": {
                "custom_analyzer": false,
                "analyzer": {"name": name, "tokens": tokens},
            }}),
        );
    }

    respond(&p, json!({"tokens": tokens}))
}
