//! Suggesting what the caller might have meant, or might type next.

use super::*;

/// Build the `suggest` section of a response.
///
/// Two shapes are answered. A completion suggester looks for stored values
/// that begin with what has been typed so far; a term suggester takes text
/// that may be misspelled and offers, for each word, the closest word the
/// index actually holds.
pub(crate) fn build_suggest(
    store: &Store,
    targets: &[String],
    spec: &Value,
    typed_keys: bool,
) -> std::result::Result<Value, Response> {
    let Some(named) = spec.as_object() else { return Ok(json!({})) };
    let global_text = named.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let mut out = serde_json::Map::new();

    for (name, body) in named {
        if name == "text" {
            continue;
        }
        let Some(b) = body.as_object() else { continue };
        let text = b.get("text").and_then(|t| t.as_str()).unwrap_or(global_text);

        if let Some(c) = b.get("completion") {
            let entries = completion_suggest(store, targets, text, c)?;
            let key = if typed_keys { format!("completion#{name}") } else { name.clone() };
            out.insert(key, entries);
        } else if let Some(t) = b.get("term") {
            let entries = term_suggest(store, targets, text, t)?;
            let key = if typed_keys { format!("term#{name}") } else { name.clone() };
            out.insert(key, entries);
        } else if let Some(p) = b.get("phrase") {
            // a phrase suggester is answered per whole input rather than per
            // word; the options come from the same place the terms do
            let entries = term_suggest(store, targets, text, p)?;
            let key = if typed_keys { format!("phrase#{name}") } else { name.clone() };
            out.insert(key, entries);
        }
    }
    Ok(Value::Object(out))
}

/// Values that begin with what has been typed.
/// Does this document sit in the contexts the suggestion asked for?
///
/// A completion field may be filed under contexts -- a category it belongs to,
/// or a place it is near. The values come from the completion object itself,
/// or from another field the mapping points at.
pub(crate) fn context_matches(
    g: &IdxState,
    field: &str,
    spec: &Value,
    raw: Option<&Value>,
    source: &Value,
) -> bool {
    let Some(asked) = spec.get("contexts").and_then(|c| c.as_object()) else { return true };
    let path_of = match field.rsplit_once('.') {
        Some((parent, leaf)) => format!(
            "/properties/{}/fields/{leaf}/contexts",
            parent.replace('.', "/properties/")
        ),
        None => format!("/properties/{field}/contexts"),
    };
    let declared = g
        .mapping
        .raw
        .pointer(&path_of)
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    for (name, want) in asked {
        let kind = declared
            .iter()
            .find(|d| d.get("name").and_then(|n| n.as_str()) == Some(name.as_str()))
            .and_then(|d| d.get("type").and_then(|t| t.as_str()))
            .unwrap_or("category");
        let path = declared
            .iter()
            .find(|d| d.get("name").and_then(|n| n.as_str()) == Some(name.as_str()))
            .and_then(|d| d.get("path").and_then(|t| t.as_str()));
        // the document's own value for this context: written beside the
        // completion, or read from the field the mapping names
        let held = raw
            .and_then(|r| r.pointer(&format!("/contexts/{name}")))
            .or_else(|| {
                path.and_then(|p| source.pointer(&format!("/{}", p.replace('.', "/"))))
            });
        let Some(held) = held else { return false };
        let ok = if kind == "geo" {
            let precision = declared
                .iter()
                .find(|d| d.get("name").and_then(|n| n.as_str()) == Some(name.as_str()))
                .and_then(|d| d.get("precision"))
                .and_then(|v| v.as_str())
                .and_then(parse_distance)
                .unwrap_or(5_000.0);
            let wanted = want.get("context").unwrap_or(want);
            geo_distance_metres(wanted, held).map(|d| d <= precision).unwrap_or(false)
        } else {
            let listed = |v: &Value| -> Vec<String> {
                match v {
                    Value::String(s) => vec![s.clone()],
                    Value::Array(a) => a
                        .iter()
                        .map(|x| match x.get("context").unwrap_or(x) {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .collect(),
                    Value::Object(o) => o
                        .get("context")
                        .map(|c| match c {
                            Value::String(s) => vec![s.clone()],
                            other => vec![other.to_string()],
                        })
                        .unwrap_or_default(),
                    other => vec![other.to_string()],
                }
            };
            let wants = listed(want);
            let has = listed(held);
            wants.iter().any(|w| has.contains(w))
        };
        if !ok {
            return false;
        }
    }
    true
}

pub(crate) fn completion_suggest(
    store: &Store,
    targets: &[String],
    text: &str,
    spec: &Value,
) -> std::result::Result<Value, Response> {
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    let size = spec.get("size").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
    let skip_duplicates = spec
        .get("skip_duplicates")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let prefix = text.to_lowercase();

    // a field filed under contexts can only be asked about through them
    let contexts_path = |field: &str| -> String {
        match field.rsplit_once('.') {
            // a sub-field lives under its parent's `fields`
            Some((parent, leaf)) => format!(
                "/properties/{}/fields/{leaf}/contexts",
                parent.replace('.', "/properties/")
            ),
            None => format!("/properties/{field}/contexts"),
        }
    };
    for name in targets {
        let Some(st) = store.get(name) else { continue };
        let g = st.read();
        let declared = g
            .mapping
            .raw
            .pointer(&contexts_path(&field))
            .and_then(|c| c.as_array())
            .map(|c| !c.is_empty())
            .unwrap_or(false);
        // an empty contexts clause names none of them, which is the same as
        // not naming any
        let named_none = match spec.get("contexts") {
            None => true,
            Some(Value::Object(o)) => {
                o.is_empty()
                    || o.values().any(|v| v.as_array().map(|a| a.is_empty()).unwrap_or(false))
            }
            _ => false,
        };
        if declared && named_none {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("Missing mandatory contexts in context query for field [{field}]"),
            ));
        }
    }

    let mut options: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = Default::default();
    for name in targets {
        let Some(st) = store.get(name) else { continue };
        let g = st.read();
        let searcher = g.reader.searcher();
        let all = boostcore::query::AllQuery;
        let addrs = searcher
            .search(&all, &boostcore::collector::DocSetCollector)
            .map_err(|e| {
                err(StatusCode::BAD_REQUEST, "search_phase_execution_exception", e.to_string())
            })?;
        for addr in addrs {
            let Some((id, source)) = source_of(&searcher, &g, addr) else { continue };
            // a completion field may hold one value or several
            // a completion value may be plain text, a list of them, or an
            // object carrying the inputs and the weight to rank them by
            // a completion may be declared as a sub-field of another, and
            // then the value it completes is the parent's
            let raw = source.pointer(&format!("/{}", field.replace('.', "/"))).or_else(|| {
                field
                    .rsplit_once('.')
                    .and_then(|(parent, _)| {
                        source.pointer(&format!("/{}", parent.replace('.', "/")))
                    })
            });
            let texts = |v: &Value| -> Vec<String> {
                match v {
                    Value::String(s) => vec![s.clone()],
                    Value::Array(a) => {
                        a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()
                    }
                    _ => Vec::new(),
                }
            };
            // ... and a list may hold several such objects, each with its own
            // weight
            let mut weighted: Vec<(String, f64)> = Vec::new();
            match raw {
                Some(Value::Object(o)) => {
                    let w = o.get("weight").and_then(|x| x.as_f64()).unwrap_or(1.0);
                    weighted.extend(
                        o.get("input").map(&texts).unwrap_or_default().into_iter().map(|t| (t, w)),
                    );
                }
                Some(Value::Array(items)) if items.iter().any(|i| i.is_object()) => {
                    for item in items {
                        let Some(o) = item.as_object() else {
                            weighted.extend(texts(item).into_iter().map(|t| (t, 1.0)));
                            continue;
                        };
                        let w = o.get("weight").and_then(|x| x.as_f64()).unwrap_or(1.0);
                        weighted.extend(
                            o.get("input")
                                .map(&texts)
                                .unwrap_or_default()
                                .into_iter()
                                .map(|t| (t, w)),
                        );
                    }
                }
                Some(other) => weighted.extend(texts(other).into_iter().map(|t| (t, 1.0))),
                None => {}
            }
            if !context_matches(&g, &field, spec, raw, &source) {
                continue;
            }
            for (v, weight) in weighted {
                if !v.to_lowercase().starts_with(&prefix) {
                    continue;
                }
                if skip_duplicates && !seen.insert(v.clone()) {
                    continue;
                }
                options.push(json!({
                    "text": v,
                    "_index": g.name,
                    "_id": id,
                    "_score": weight,
                    "_source": source,
                }));
            }
        }
    }
    // the heavier suggestion comes first; the text settles equal weights
    options.sort_by(|a, b| {
        let w = |v: &Value| v.get("_score").and_then(|s| s.as_f64()).unwrap_or(1.0);
        let t = |v: &Value| v.get("text").and_then(|s| s.as_str()).unwrap_or("").to_string();
        w(b).partial_cmp(&w(a)).unwrap_or(Ordering::Equal).then_with(|| t(a).cmp(&t(b)))
    });
    options.truncate(size);
    Ok(json!([{
        "text": text,
        "offset": 0,
        "length": text.chars().count(),
        "options": options,
    }]))
}

/// For each word, the closest word the index holds.
pub(crate) fn term_suggest(
    store: &Store,
    targets: &[String],
    text: &str,
    spec: &Value,
) -> std::result::Result<Value, Response> {
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    let size = spec.get("size").and_then(|v| v.as_u64()).unwrap_or(5) as usize;

    // every word the field actually holds, gathered once
    let mut vocabulary: std::collections::HashSet<String> = Default::default();
    for name in targets {
        let Some(st) = store.get(name) else { continue };
        let g = st.read();
        let searcher = g.reader.searcher();
        let all = boostcore::query::AllQuery;
        let Ok(addrs) = searcher.search(&all, &boostcore::collector::DocSetCollector) else {
            continue;
        };
        for addr in addrs {
            let Some((_, source)) = source_of(&searcher, &g, addr) else { continue };
            let Some(v) = source.pointer(&format!("/{}", field.replace('.', "/"))) else {
                continue;
            };
            let texts: Vec<String> = match v {
                Value::String(s) => vec![s.clone()],
                Value::Array(a) => {
                    a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()
                }
                _ => Vec::new(),
            };
            for t in texts {
                for word in t.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
                    vocabulary.insert(word.to_lowercase());
                }
            }
        }
    }

    let mut entries = Vec::new();
    let mut offset = 0usize;
    for word in text.split_whitespace() {
        let start = text[offset..].find(word).map(|i| offset + i).unwrap_or(offset);
        offset = start + word.len();
        let lower = word.to_lowercase();
        // a word the index already has needs no correction
        let mut options: Vec<(usize, String)> = if vocabulary.contains(&lower) {
            Vec::new()
        } else {
            vocabulary
                .iter()
                .filter_map(|cand| {
                    let d = edit_distance(&lower, cand);
                    (d > 0 && d <= 2).then(|| (d, cand.clone()))
                })
                .collect()
        };
        options.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        options.truncate(size);
        entries.push(json!({
            "text": word,
            "offset": start,
            "length": word.chars().count(),
            "options": options
                .into_iter()
                .map(|(d, t)| json!({
                    "text": t,
                    "score": 1.0 - (d as f64) / 10.0,
                    "freq": 1,
                }))
                .collect::<Vec<_>>(),
        }));
    }
    Ok(Value::Array(entries))
}

/// How many single-character edits turn one word into the other.
pub(crate) fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}
