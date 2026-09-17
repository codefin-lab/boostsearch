//! Marking, in the text a document holds, the words a query asked for.
//!
//! Three highlighters, as upstream has them. `unified` (the default) joins a
//! field's values into one text and returns the best passages of it; `plain`
//! runs over one value at a time and returns the best fragments of each;
//! `fvh` reads the field's term vectors and marks whole phrases. All three
//! cut what they return to `fragment_size` and `number_of_fragments`; the
//! node used to return every value whole, whatever the request asked for.

mod annotated;
mod breaks;
mod fvh;
mod plain;
mod query;
mod text;
mod unified;

pub(crate) use annotated::{query_terms_by_field, without_markup};

use query::{Extract, Hq, Leaf};
use serde_json::{Value, json};
use text::{Env, Tok, values_of};

/// How the text between the marks is written out.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Encoder {
    Default,
    Html,
}

impl Encoder {
    pub(crate) fn encode(self, s: &str) -> String {
        match self {
            Encoder::Default => s.to_string(),
            Encoder::Html => {
                let mut out = String::with_capacity(s.len());
                for c in s.chars() {
                    match c {
                        '"' => out.push_str("&quot;"),
                        '&' => out.push_str("&amp;"),
                        '<' => out.push_str("&lt;"),
                        '>' => out.push_str("&gt;"),
                        '\'' => out.push_str("&#x27;"),
                        '/' => out.push_str("&#x2F;"),
                        c => out.push(c),
                    }
                }
                out
            }
        }
    }
}

const STYLED: [&str; 10] = [
    "<em class=\"hlt1\">",
    "<em class=\"hlt2\">",
    "<em class=\"hlt3\">",
    "<em class=\"hlt4\">",
    "<em class=\"hlt5\">",
    "<em class=\"hlt6\">",
    "<em class=\"hlt7\">",
    "<em class=\"hlt8\">",
    "<em class=\"hlt9\">",
    "<em class=\"hlt10\">",
];

/// The options one field is highlighted with: its own, then the request's,
/// then the defaults.
#[derive(Clone, Debug)]
pub(crate) struct Opts {
    pub(crate) kind: String,
    pub(crate) pre_tags: Vec<String>,
    pub(crate) post_tags: Vec<String>,
    pub(crate) score_order: bool,
    pub(crate) fragment_size: i64,
    pub(crate) fragments: i64,
    pub(crate) no_match_size: i64,
    pub(crate) fragmenter: Option<String>,
    pub(crate) boundary_scanner: Option<String>,
    pub(crate) boundary_chars: Vec<u16>,
    pub(crate) boundary_max_scan: usize,
    pub(crate) fragment_offset: Option<i64>,
    pub(crate) matched_fields: Vec<String>,
    pub(crate) require_field_match: bool,
    pub(crate) highlight_query: Option<Value>,
    pub(crate) max_analyzer_offset: Option<usize>,
    pub(crate) phrase_limit: usize,
}

fn tags(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::Array(a) => {
            let t: Vec<String> = a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
            (!t.is_empty()).then_some(t)
        }
        Value::String(s) => Some(vec![s.clone()]),
        _ => None,
    }
}

fn int(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.parse().ok())
}

impl Opts {
    /// The options of one field of the request.
    pub(crate) fn of(spec: &Value, field: &Value) -> Opts {
        let mut o = Opts {
            kind: "unified".to_string(),
            pre_tags: vec!["<em>".to_string()],
            post_tags: vec!["</em>".to_string()],
            score_order: false,
            fragment_size: 100,
            fragments: 5,
            no_match_size: 0,
            fragmenter: None,
            boundary_scanner: None,
            boundary_chars: ".,!? \t\n".encode_utf16().collect(),
            boundary_max_scan: 20,
            fragment_offset: None,
            matched_fields: Vec::new(),
            require_field_match: true,
            highlight_query: None,
            max_analyzer_offset: None,
            phrase_limit: 256,
        };
        // the request's options first and the field's over them; within
        // either, a later key wins over an earlier one, which is how
        // `tags_schema` and `pre_tags` settle it upstream
        for level in [spec, field] {
            let Some(map) = level.as_object() else { continue };
            for (key, v) in map {
                match key.as_str() {
                    "type" => o.kind = v.as_str().unwrap_or("unified").to_string(),
                    "pre_tags" => o.pre_tags = tags(v).unwrap_or(o.pre_tags),
                    "post_tags" => o.post_tags = tags(v).unwrap_or(o.post_tags),
                    "tags_schema" if std::ptr::eq(level, spec) => {
                        if v.as_str() == Some("styled") {
                            o.pre_tags = STYLED.iter().map(|s| s.to_string()).collect();
                            o.post_tags = vec!["</em>".to_string()];
                        } else if v.as_str() == Some("default") {
                            o.pre_tags = vec!["<em>".to_string()];
                            o.post_tags = vec!["</em>".to_string()];
                        }
                    }
                    "order" => o.score_order = v.as_str() == Some("score"),
                    "fragment_size" => o.fragment_size = int(v).unwrap_or(o.fragment_size),
                    "number_of_fragments" => o.fragments = int(v).unwrap_or(o.fragments),
                    "no_match_size" => o.no_match_size = int(v).unwrap_or(0),
                    "fragmenter" => o.fragmenter = v.as_str().map(str::to_string),
                    "boundary_scanner" => {
                        o.boundary_scanner = v.as_str().map(|s| s.to_ascii_lowercase())
                    }
                    "boundary_chars" => {
                        if let Some(s) = v.as_str() {
                            o.boundary_chars = s.encode_utf16().collect();
                        }
                    }
                    "boundary_max_scan" => {
                        o.boundary_max_scan = int(v).unwrap_or(20).max(0) as usize
                    }
                    "fragment_offset" if std::ptr::eq(level, field) => o.fragment_offset = int(v),
                    "matched_fields" if std::ptr::eq(level, field) => {
                        o.matched_fields = match v {
                            Value::Array(a) => {
                                a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()
                            }
                            Value::String(s) => vec![s.clone()],
                            _ => Vec::new(),
                        }
                    }
                    "require_field_match" => {
                        o.require_field_match =
                            v.as_bool().or_else(|| v.as_str().map(|s| s == "true")).unwrap_or(true)
                    }
                    "highlight_query" => o.highlight_query = Some(v.clone()),
                    "max_analyzer_offset" => {
                        o.max_analyzer_offset = int(v).map(|n| n.max(0) as usize)
                    }
                    "phrase_limit" => o.phrase_limit = int(v).unwrap_or(256).max(0) as usize,
                    _ => {}
                }
            }
        }
        o
    }
}

/// The fields a request names, each with its own options.
fn field_patterns(spec: &Value) -> Vec<(String, Value)> {
    match spec.get("fields") {
        Some(Value::Object(o)) => o.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|f| f.as_object())
            .flat_map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>())
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether a field keeps term vectors with offsets, which is what the fast
/// vector highlighter reads.
fn has_offset_vectors(mapping: &crate::store::Mapping, field: &str) -> bool {
    mapping
        .field_option(field, "term_vector")
        .and_then(|v| v.as_str().map(|s| s.contains("offsets")))
        .unwrap_or(false)
}

fn is_highlightable_type(t: &str) -> bool {
    matches!(t, "text" | "keyword" | "match_only_text")
}

/// Why the request's highlighting cannot be done, where it cannot: the
/// errors upstream raises while it sets a highlighter up for a field, before
/// it reads any value. Asked only when there are hits to highlight.
pub(crate) fn highlight_refusal(spec: &Value, mapping: &crate::store::Mapping) -> Option<String> {
    for (pattern, field) in field_patterns(spec) {
        let opts = Opts::of(spec, &field);
        if !matches!(opts.kind.as_str(), "unified" | "plain" | "fvh" | "annotated") {
            return Some(format!(
                "unknown highlighter type [{}] for the field [{pattern}]",
                opts.kind
            ));
        }
        let names: Vec<String> = if pattern.contains('*') {
            let mut n: Vec<String> = mapping
                .types
                .iter()
                .filter(|(k, t)| crate::store::glob_match(&pattern, k) && is_highlightable_type(t))
                .map(|(k, _)| k.clone())
                .collect();
            n.sort();
            n
        } else {
            vec![pattern.clone()]
        };
        for name in names {
            let Some(kind) = mapping.type_of(&name).map(str::to_string).or_else(|| {
                mapping.field_option(&name, "type").and_then(|t| t.as_str().map(str::to_string))
            }) else {
                continue;
            };
            let tokenized = !matches!(kind.as_str(), "keyword" | "constant_keyword" | "wildcard");
            match opts.kind.as_str() {
                "fvh" => {
                    if !has_offset_vectors(mapping, &name) {
                        if pattern.contains('*') {
                            continue;
                        }
                        return Some(format!(
                            "the field [{name}] should be indexed with term vector with position offsets to be \
                             used with fast vector highlighter"
                        ));
                    }
                    if opts.fragments != 0 {
                        let margin = opts.fragment_offset.unwrap_or(6).max(0);
                        let least = (margin * 3).max(1);
                        if opts.fragment_size < least {
                            return Some(format!(
                                "fragCharSize({}) is too small. It must be {least} or higher.",
                                opts.fragment_size
                            ));
                        }
                    }
                }
                "plain" => {
                    if opts.fragments != 0
                        && let Some(f) = opts.fragmenter.as_deref()
                        && f != "simple"
                        && f != "span"
                    {
                        return Some(format!(
                            "unknown fragmenter option [{f}] for the field [{name}]"
                        ));
                    }
                }
                "unified" => {
                    if opts.fragments != 0
                        && tokenized
                        && let Some(b) = opts.boundary_scanner.as_deref()
                        && b != "sentence"
                        && b != "word"
                    {
                        return Some(format!("Invalid boundary scanner type: {b}"));
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Whether a leaf asked of `leaf_field` applies to the field being read.
fn field_applies(leaf_field: &str, field: &str) -> bool {
    leaf_field == field || leaf_field == "*" || crate::store::glob_match(leaf_field, field)
}

/// Whether a query's terms match wherever they stand for the unified
/// highlighter: terms, and alternatives or exclusions over nothing but terms.
fn loose(q: &Hq) -> bool {
    match q {
        Hq::Leaf { .. } | Hq::Loose(_) => true,
        Hq::Or(clauses) => clauses.iter().all(loose),
        Hq::Not { include, .. } => loose(include),
        _ => false,
    }
}

/// The matches the unified highlighter finds in one value's tokens.
fn unified_matches(
    queries: &[Hq],
    toks: &[Tok],
    field: &str,
    require: bool,
    out: &mut Vec<unified::Match>,
) {
    let ok = |f: &str| !require || field_applies(f, field);
    for q in queries {
        match q {
            // A `span_or` or a `span_not` over plain terms is read by the
            // unified highlighter as the terms it holds: the reference marks
            // every `notice` for a `span_not` of `notice`, the excluded one
            // too, where a `span_first` of it marks only the first.
            q if loose(q) => {
                let mut leaves = Vec::new();
                q.leaves(&mut leaves);
                for (f, leaf, _) in &leaves {
                    if !ok(f) {
                        continue;
                    }
                    for t in toks.iter().filter(|t| leaf.matches(&t.term)) {
                        out.push(unified::Match {
                            start: t.from,
                            end: t.to,
                            key: leaf.key(&t.term),
                            freq: 0,
                        });
                    }
                }
            }
            positional => {
                let mut leaves = Vec::new();
                positional.leaves(&mut leaves);
                for span in query::spans(positional, toks, &ok) {
                    for &i in &span.tokens {
                        let t = &toks[i];
                        for (f, leaf, _) in &leaves {
                            if ok(f) && leaf.matches(&t.term) {
                                out.push(unified::Match {
                                    start: t.from,
                                    end: t.to,
                                    key: leaf.key(&t.term),
                                    freq: 0,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
}

/// What the plain highlighter's scorer knows of one value.
fn plain_scored(queries: &[Hq], toks: &[Tok], field: &str, require: bool) -> plain::Scored {
    let ok = |f: &str| !require || field_applies(f, field);
    let mut scored = plain::Scored::new();
    for q in queries {
        match q {
            Hq::Leaf { .. } | Hq::Loose(_) => {
                let mut leaves = Vec::new();
                q.leaves(&mut leaves);
                for (f, leaf, boost) in &leaves {
                    if !ok(f) {
                        continue;
                    }
                    let terms: Vec<String> = match leaf {
                        Leaf::Term(t) => vec![t.clone()],
                        multi => toks
                            .iter()
                            .filter(|t| multi.matches(&t.term))
                            .map(|t| t.term.clone())
                            .collect(),
                    };
                    for term in terms {
                        let entry = scored.entry(term).or_insert(plain::Weighted {
                            weight: *boost,
                            positional: false,
                            spans: Vec::new(),
                        });
                        entry.positional = false;
                        entry.weight = *boost;
                    }
                }
            }
            positional => {
                let found: Vec<(usize, usize)> = query::spans(positional, toks, &ok)
                    .iter()
                    .map(|s| (s.start, s.end.saturating_sub(1)))
                    .collect();
                let mut leaves = Vec::new();
                positional.leaves(&mut leaves);
                for (f, leaf, boost) in leaves {
                    if !ok(&f) {
                        continue;
                    }
                    let terms: Vec<String> = match &leaf {
                        Leaf::Term(t) => vec![t.clone()],
                        multi => toks
                            .iter()
                            .filter(|t| multi.matches(&t.term))
                            .map(|t| t.term.clone())
                            .collect(),
                    };
                    for term in terms {
                        let entry = scored.entry(term).or_insert(plain::Weighted {
                            weight: boost,
                            positional: true,
                            spans: Vec::new(),
                        });
                        for s in &found {
                            if !entry.spans.contains(s) {
                                entry.spans.push(*s);
                            }
                        }
                    }
                }
            }
        }
    }
    scored
}

/// The first `n` UTF-16 units of a text, as a string.
fn units_prefix(text: &str, n: usize) -> String {
    let u: Vec<u16> = text.encode_utf16().take(n).collect();
    String::from_utf16_lossy(&u)
}

pub(crate) fn build_highlight(
    spec: &Value,
    source: &Value,
    query: &Option<Value>,
    mapping: &crate::store::Mapping,
    index: &velocore::Index,
    analysis: &crate::analysis::Registry,
) -> Option<Value> {
    spec.get("fields")?;
    let env = Env { mapping, index, analysis };
    // where the document itself is not kept, only a field stored in its own
    // right has any text left to highlight
    let source_kept = mapping.raw.pointer("/_source/enabled") != Some(&json!(false));
    let patterns: Vec<(String, Value)> = field_patterns(spec)
        .into_iter()
        .filter(|(name, _)| source_kept || mapping.field_option(name, "store") == Some(json!(true)))
        .collect();
    let encoder = match spec.get("encoder").and_then(|v| v.as_str()) {
        Some("html") => Encoder::Html,
        _ => Encoder::Default,
    };
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

    let extract = Extract { env: &env };
    let asked = extract.run(query.as_ref());
    let mut out = serde_json::Map::new();
    for name in candidates {
        let Some((pattern, field_spec)) = patterns
            .iter()
            .find(|(pat, _)| pat == &name || pat == "*" || crate::store::glob_match(pat, &name))
        else {
            continue;
        };
        let opts = Opts::of(spec, field_spec);
        // a field reached through a pattern is highlighted only when it is
        // text or a keyword, and when the highlighter can read it
        if pattern.contains('*') {
            if let Some(t) = env.type_of(&name)
                && !is_highlightable_type(&t)
                && !derived_type(mapping, &name)
            {
                continue;
            }
            if opts.kind == "fvh" && !has_offset_vectors(mapping, &name) {
                continue;
            }
        }
        // the value lives at the field's own path, or at its parent's when the
        // field is a multi-field of another
        let mut values = values_of(source, &name);
        if values.is_empty()
            && let Some((parent, _)) = name.rsplit_once('.')
        {
            values = values_of(source, parent);
        }
        // a value longer than `ignore_above` was never indexed, so there is
        // nothing in it that could have matched
        if let Some(limit) = mapping.field_option(&name, "ignore_above").and_then(|v| v.as_u64()) {
            values.retain(|v| v.chars().count() as u64 <= limit);
        }
        if values.is_empty() {
            continue;
        }
        let own;
        let queries: &[Hq] = match &opts.highlight_query {
            Some(q) => {
                own = extract.run(Some(q));
                &own
            }
            None => &asked,
        };
        let fragments: Vec<String> = match opts.kind.as_str() {
            "annotated" => {
                annotated::highlight(&values, &name, &opts, query, mapping, index, analysis)
            }
            "plain" => plain_field(&env, &name, &values, queries, &opts, encoder),
            "fvh" => fvh_field(&env, &name, &values, queries, &opts, encoder),
            _ => unified_field(&env, &name, &values, queries, &opts, encoder),
        };
        if !fragments.is_empty() {
            out.insert(name, json!(fragments));
        }
    }
    (!out.is_empty()).then(|| Value::Object(out))
}

fn derived_type(mapping: &crate::store::Mapping, name: &str) -> bool {
    mapping.derived_fields().iter().any(|(n, _)| n == name)
}

fn unified_field(
    env: &Env,
    name: &str,
    values: &[String],
    queries: &[Hq],
    opts: &Opts,
    encoder: Encoder,
) -> Vec<String> {
    let mut content: Vec<u16> = Vec::new();
    let mut bases = Vec::new();
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            content.push(0);
        }
        bases.push(content.len());
        content.extend(v.encode_utf16());
    }
    let mut fields = vec![name.to_string()];
    for f in &opts.matched_fields {
        if !fields.contains(f) {
            fields.push(f.clone());
        }
    }
    let mut matches: Vec<unified::Match> = Vec::new();
    for field in &fields {
        for (v, base) in values.iter().zip(&bases) {
            let mut toks = env.index_tokens(field, v);
            // `max_analyzer_offset` says how far into a value the analyzer
            // reads: a token starting past it is never seen
            if let Some(limit) = opts.max_analyzer_offset {
                toks.retain(|t| t.from <= limit);
            }
            for t in toks.iter_mut() {
                t.from += base;
                t.to += base;
            }
            unified_matches(queries, &toks, field, opts.require_field_match, &mut matches);
        }
    }
    // one match per term at a place, however many clauses asked for it
    matches.sort_by(|a, b| (a.start, a.end, &a.key).cmp(&(b.start, b.end, &b.key)));
    matches.dedup_by(|a, b| a.start == b.start && a.end == b.end && a.key == b.key);
    let mut freq: std::collections::HashMap<String, usize> = Default::default();
    for m in &matches {
        *freq.entry(m.key.clone()).or_default() += 1;
    }
    for m in matches.iter_mut() {
        m.freq = freq[&m.key];
    }
    let tokenized = !env.is_keyword(name);
    let mut snippets = unified::highlight(&content, &mut matches, opts, tokenized, encoder);
    snippets.retain(|(s, _)| s.chars().any(|c| !c.is_whitespace()));
    if opts.score_order {
        snippets.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    }
    snippets.into_iter().map(|(s, _)| s).collect()
}

fn plain_field(
    env: &Env,
    name: &str,
    values: &[String],
    queries: &[Hq],
    opts: &Opts,
    encoder: Encoder,
) -> Vec<String> {
    let mut list: Vec<plain::Fragment> = Vec::new();
    for v in values {
        // the plain highlighter reads only as far as it was told to, and
        // what it returns is made of what it read
        let text = match opts.max_analyzer_offset {
            Some(limit) if v.encode_utf16().count() > limit => units_prefix(v, limit),
            _ => v.clone(),
        };
        let units: Vec<u16> = text.encode_utf16().collect();
        let toks = env.index_tokens(name, &text);
        let scored = plain_scored(queries, &toks, name, opts.require_field_match);
        list.extend(
            plain::fragments(&units, &toks, &scored, opts, encoder)
                .into_iter()
                .filter(|f| f.score > 0.0),
        );
    }
    if opts.score_order {
        // upstream sorts with `Math.round(b - a)`, so scores less than half
        // apart keep the order they came in
        for i in 1..list.len() {
            let mut j = i;
            while j > 0 && (list[j].score - list[j - 1].score).round() > 0.0 {
                list.swap(j, j - 1);
                j -= 1;
            }
        }
    }
    let take = if opts.fragments == 0 && values.len() > 1 {
        list.len()
    } else {
        list.len().min(opts.fragments.max(1) as usize)
    };
    if take > 0 {
        return list.into_iter().take(take).map(|f| f.text).collect();
    }
    if opts.no_match_size > 0 {
        let first = &values[0];
        let len = first.encode_utf16().count();
        let toks = env.index_tokens(name, first);
        if let Some(end) = plain::no_match_end(&toks, opts.no_match_size as usize, len)
            && end > 0
        {
            return vec![units_prefix(first, end)];
        }
    }
    Vec::new()
}

fn fvh_field(
    env: &Env,
    name: &str,
    values: &[String],
    queries: &[Hq],
    opts: &Opts,
    encoder: Encoder,
) -> Vec<String> {
    let fields: Vec<String> = if opts.matched_fields.is_empty() {
        vec![name.to_string()]
    } else {
        opts.matched_fields.clone()
    };
    let mut texts: Vec<(String, Vec<Tok>)> = Vec::new();
    for field in fields {
        if field != name && !has_offset_vectors(env.mapping, &field) {
            continue;
        }
        let mut all: Vec<Tok> = Vec::new();
        let mut offset = 0usize;
        let mut position = 0usize;
        for v in values {
            let toks = env.index_tokens(&field, v);
            let last = toks.iter().map(|t| t.pos + 1).max().unwrap_or(0);
            all.extend(toks.into_iter().map(|t| Tok {
                from: t.from + offset,
                to: t.to + offset,
                pos: t.pos + position,
                ..t
            }));
            offset += v.encode_utf16().count() + 1;
            position += last + 100;
        }
        texts.push((field, all));
    }
    let flats = fvh::flatten(queries, &texts);
    fvh::highlight(values, &texts, &flats, opts, encoder)
}
