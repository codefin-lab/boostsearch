//! Correcting a whole phrase rather than one word at a time.
//!
//! A term suggester answers for each word on its own, which is why it will
//! happily offer a word that no one has ever written next to its neighbour. A
//! phrase suggester weighs the whole line: it collects what each word could
//! have been, builds the lines those choices make, and prefers the line whose
//! neighbouring words the index has actually seen together.

use super::*;

/// One word the input could have been, and what it cost to get there.
struct Candidate {
    word: String,
    /// how many single-character edits away from what was written
    cost: usize,
}

/// `phrase` -- the whole input, corrected.
pub(crate) fn phrase_suggest(
    store: &Store,
    targets: &[String],
    text: &str,
    spec: &Value,
) -> std::result::Result<Value, Response> {
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    let size = spec.get("size").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
    let force_unigrams = spec.get("force_unigrams").and_then(|v| v.as_bool()).unwrap_or(true);
    let separator = spec.get("separator").and_then(|v| v.as_str()).unwrap_or(" ").to_string();

    let Some(name) = targets.first() else { return Ok(json!([])) };
    let Some(st) = store.get(name) else { return Ok(json!([])) };
    let g = st.read();

    // the analyzer the caller named, or the one the field is written with
    let analyzer = spec
        .get("analyzer")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| analyzer_of(&g, &field));
    let cut = analysed_at(&g, analyzer.as_deref(), text);
    // a chain that joins words into shingles and keeps none of them whole has
    // nothing to correct: every token it emits is already a pair
    let mut words: Vec<String> = Vec::new();
    let mut last: Option<usize> = None;
    let mut whole_everywhere = true;
    for (at, tokens) in group_positions(&cut) {
        match tokens.iter().find(|w| !w.contains(' ')) {
            Some(word) => words.push(word.clone()),
            None => whole_everywhere = false,
        }
        last = Some(at);
    }
    let _ = last;
    if force_unigrams && !whole_everywhere {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "The field [{field}] can not be used with a phrase suggester since it doesn't \
                 emit unigrams"
            ),
        ));
    }
    if words.is_empty() {
        return Ok(json!([]));
    }

    // how many of the words may be wrong: a fraction of them, or a count
    let max_errors = spec.get("max_errors").and_then(|v| v.as_f64()).unwrap_or(1.0);
    let allowed = match max_errors < 1.0 {
        true => ((max_errors * words.len() as f64).ceil() as usize).max(1),
        false => max_errors as usize,
    };

    let generators = match spec.get("direct_generator").and_then(|v| v.as_array()) {
        Some(list) if !list.is_empty() => list.clone(),
        _ => vec![json!({"field": field})],
    };

    // what the field itself holds, which says whether a word stands at all
    // and whether two of them stand together
    let known = terms_of(&g, &field);

    let mut choices: Vec<Vec<Candidate>> = Vec::new();
    for word in &words {
        let mut here: Vec<Candidate> = Vec::new();
        if known.contains(word) {
            here.push(Candidate { word: word.clone(), cost: 0 });
        }
        for generator in &generators {
            for found in generated(&g, generator, word) {
                if here.iter().any(|c| c.word == found.word) {
                    continue;
                }
                here.push(found);
            }
        }
        // a word nothing can be said about stands as it was written
        if here.is_empty() {
            here.push(Candidate { word: word.clone(), cost: 0 });
        }
        here.sort_by(|a, b| a.cost.cmp(&b.cost).then(a.word.cmp(&b.word)));
        here.truncate(4);
        choices.push(here);
    }

    // every line those choices make, up to the point where there are more of
    // them than anyone would read
    const MOST: usize = 4096;
    let mut lines: Vec<(Vec<String>, usize)> = vec![(Vec::new(), 0)];
    for here in &choices {
        let mut next = Vec::new();
        for (line, cost) in &lines {
            for candidate in here {
                if cost + candidate.cost > allowed {
                    continue;
                }
                let mut longer = line.clone();
                longer.push(candidate.word.clone());
                next.push((longer, cost + candidate.cost));
            }
        }
        next.truncate(MOST);
        lines = next;
    }

    let mut scored: Vec<(f64, String)> = lines
        .into_iter()
        .map(|(line, cost)| {
            // a pair of words the index has seen together is worth more than
            // one it has only seen apart
            let together =
                line.windows(2).filter(|pair| known.contains(&pair.join(&separator))).count();
            let score = together as f64 - cost as f64 / 10.0;
            (score, line.join(&separator))
        })
        .collect();
    let written = words.join(&separator);
    scored.retain(|(_, line)| *line != written);
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1))
    });
    scored.dedup_by(|a, b| a.1 == b.1);
    scored.truncate(size);

    Ok(json!([{
        "text": text,
        "offset": 0,
        "length": text.chars().count(),
        "options": scored
            .into_iter()
            .map(|(score, line)| json!({"text": line, "score": score.max(0.0) + 0.01}))
            .collect::<Vec<_>>(),
    }]))
}

/// The words one generator offers for a word that was written.
///
/// A generator may read a field written another way round -- reversed, folded
/// -- in which case the word is put through the same filter before it is
/// looked for, and what is found is put back through the other one.
fn generated(g: &IdxState, generator: &Value, word: &str) -> Vec<Candidate> {
    let field = generator.get("field").and_then(|v| v.as_str()).unwrap_or_default();
    let least = generator.get("min_word_length").and_then(|v| v.as_u64()).unwrap_or(4) as usize;
    if word.chars().count() < least {
        return Vec::new();
    }
    let edits = generator.get("max_edits").and_then(|v| v.as_u64()).unwrap_or(2) as usize;
    let prefix = generator.get("prefix_length").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
    let mode = generator.get("suggest_mode").and_then(|v| v.as_str()).unwrap_or("missing");
    let before = generator.get("pre_filter").and_then(|v| v.as_str());
    let after = generator.get("post_filter").and_then(|v| v.as_str());

    let looked_for = match before {
        Some(named) => analysed(g, Some(named), word).first().cloned().unwrap_or_default(),
        None => word.to_string(),
    };
    let vocabulary = terms_of(g, field);
    // a word the index already holds needs no correction unless the caller
    // asked for one anyway
    if mode != "always" && vocabulary.contains(&looked_for) {
        return Vec::new();
    }
    let head: String = looked_for.chars().take(prefix).collect();
    let mut out = Vec::new();
    for candidate in vocabulary.iter() {
        if !candidate.starts_with(&head) || candidate.contains(' ') {
            continue;
        }
        let cost = crate::search::edit_distance(&looked_for, candidate);
        if cost == 0 || cost > edits {
            continue;
        }
        let written = match after {
            Some(named) => analysed(g, Some(named), candidate).first().cloned().unwrap_or_default(),
            None => candidate.clone(),
        };
        if written.is_empty() {
            continue;
        }
        out.push(Candidate { word: written, cost });
    }
    out.sort_by(|a, b| a.cost.cmp(&b.cost).then(a.word.cmp(&b.word)));
    out.truncate(8);
    out
}

/// The analyzer a field is written with, where its mapping names one.
fn analyzer_of(g: &IdxState, field: &str) -> Option<String> {
    g.mapping.analyzed_paths().into_iter().find(|(path, _)| path == field).map(|(_, a)| a)
}

/// The same, keeping the place each token stands in.
fn analysed_at(g: &IdxState, analyzer: Option<&str>, text: &str) -> Vec<(String, usize)> {
    match analyzer.and_then(|named| g.analysis.get(named)) {
        Some(chain) => chain.tokens(text).into_iter().map(|(t, p, _, _)| (t, p)).collect(),
        None => analysed(g, None, text).into_iter().enumerate().map(|(p, t)| (t, p)).collect(),
    }
}

/// The tokens standing in each place, in the order the places come.
fn group_positions(tokens: &[(String, usize)]) -> Vec<(usize, Vec<String>)> {
    let mut out: Vec<(usize, Vec<String>)> = Vec::new();
    for (word, at) in tokens {
        match out.last_mut() {
            Some((before, group)) if before == at => group.push(word.clone()),
            _ => out.push((*at, vec![word.clone()])),
        }
    }
    out
}

/// The text cut into tokens by one analyzer.
fn analysed(g: &IdxState, analyzer: Option<&str>, text: &str) -> Vec<String> {
    match analyzer.and_then(|named| g.analysis.get(named)) {
        Some(chain) => chain.terms(text),
        None => text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(|w| w.to_lowercase())
            .collect(),
    }
}

/// Every token one field holds, read out of the index itself.
fn terms_of(g: &IdxState, field: &str) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let searcher = g.reader.searcher();
    let dyn_field = g.fields.dynamic;
    let path = field.replace('.', "\u{1}");
    let mut start = boostcore::Term::from_field_json_path(dyn_field, &path, true);
    start.append_type_and_str("");
    let prefix = start.serialized_value_bytes().to_vec();
    for reader in searcher.segment_readers() {
        let Ok(inverted) = reader.inverted_index(dyn_field) else { continue };
        let Ok(mut stream) = inverted.terms().stream() else { continue };
        while let Some((bytes, _)) = stream.next() {
            if !bytes.starts_with(&prefix) {
                continue;
            }
            if let Ok(word) = std::str::from_utf8(&bytes[prefix.len()..]) {
                out.insert(word.to_string());
            }
        }
    }
    out
}
