//! What a query asks of a field's text, in the shape the highlighters read.
//!
//! A highlighter does not run the query; it asks which words of one value the
//! query would have matched. Terms match wherever they stand. Phrases and span
//! queries match only where their words stand in the right places, so a
//! `match_phrase` for *other party* marks *the other party* and leaves the
//! lone *party* of *Either party* alone -- the node used to mark both.

use super::text::{Env, Tok};
use serde_json::Value;

/// One word, or one pattern over words, that a query looks for.
#[derive(Clone, Debug)]
pub(super) enum Leaf {
    Term(String),
    Prefix(String),
    /// a wildcard or a regexp, and the name the unified highlighter gives
    /// the automaton it becomes
    Pattern(regex::Regex, String),
    Fuzzy {
        term: String,
        edits: usize,
        prefix: usize,
    },
}

impl Leaf {
    pub(super) fn matches(&self, term: &str) -> bool {
        match self {
            Leaf::Term(t) => t == term,
            Leaf::Prefix(p) => term.starts_with(p.as_str()),
            Leaf::Pattern(re, _) => re.is_match(term),
            Leaf::Fuzzy { term: t, edits, prefix } => {
                let head: String = t.chars().take(*prefix).collect();
                term.starts_with(&head) && damerau(t, term) <= *edits
            }
        }
    }

    /// What the unified highlighter counts a match of this leaf as: the term
    /// itself, or for a pattern the one automaton every term it matched came
    /// out of.
    pub(super) fn key(&self, term: &str) -> String {
        match self {
            Leaf::Term(_) => term.to_string(),
            Leaf::Prefix(p) => format!("\u{1}prefix:{p}"),
            Leaf::Pattern(_, name) => format!("\u{1}pattern:{name}"),
            Leaf::Fuzzy { term, .. } => format!("\u{1}fuzzy:{term}"),
        }
    }
}

/// Edits between two words, a swap of neighbours counting as one.
fn damerau(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[n][m]
}

/// A query, as far as highlighting it goes.
#[derive(Clone, Debug)]
pub(super) enum Hq {
    Leaf {
        field: String,
        leaf: Leaf,
        boost: f32,
    },
    /// a phrase or a `span_near`: the clauses within `slop` of each other
    Near {
        clauses: Vec<Hq>,
        slop: usize,
        ordered: bool,
        phrase: bool,
        boost: f32,
    },
    Or(Vec<Hq>),
    First {
        inner: Box<Hq>,
        end: usize,
    },
    Not {
        include: Box<Hq>,
        exclude: Box<Hq>,
        pre: usize,
        post: usize,
    },
    Containing {
        big: Box<Hq>,
        little: Box<Hq>,
    },
    Within {
        big: Box<Hq>,
        little: Box<Hq>,
    },
    /// terms that match wherever they stand, though the query they came
    /// from is positional: what an interval query gives the highlighters
    Loose(Vec<Hq>),
}

impl Hq {
    /// Every leaf under this query, with the boost it carries.
    pub(super) fn leaves(&self, out: &mut Vec<(String, Leaf, f32)>) {
        match self {
            Hq::Leaf { field, leaf, boost } => out.push((field.clone(), leaf.clone(), *boost)),
            Hq::Near { clauses, .. } | Hq::Or(clauses) | Hq::Loose(clauses) => {
                clauses.iter().for_each(|c| c.leaves(out))
            }
            Hq::First { inner, .. } => inner.leaves(out),
            Hq::Not { include, .. } => include.leaves(out),
            Hq::Containing { big, little } | Hq::Within { big, little } => {
                big.leaves(out);
                little.leaves(out);
            }
        }
    }

    fn boosted(self, by: f32) -> Hq {
        if by == 1.0 {
            return self;
        }
        match self {
            Hq::Leaf { field, leaf, boost } => Hq::Leaf { field, leaf, boost: boost * by },
            Hq::Near { clauses, slop, ordered, phrase, boost } => {
                Hq::Near { clauses, slop, ordered, phrase, boost: boost * by }
            }
            other => other,
        }
    }
}

/// One stretch of positions a positional query matched, and the tokens that
/// made it.
#[derive(Clone, Debug)]
pub(super) struct Span {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) tokens: Vec<usize>,
}

/// A bound on the matches a positional query is walked for, so a long field
/// full of one word cannot make the walk explode.
const MAX_SPANS: usize = 4096;

/// Where a positional query matches in a field's tokens.
pub(super) fn spans(q: &Hq, toks: &[Tok], field_ok: &dyn Fn(&str) -> bool) -> Vec<Span> {
    let mut out = match q {
        Hq::Leaf { field, leaf, .. } => {
            if !field_ok(field) {
                return Vec::new();
            }
            toks.iter()
                .enumerate()
                .filter(|(_, t)| leaf.matches(&t.term))
                .map(|(i, t)| Span { start: t.pos, end: t.pos + t.len.max(1), tokens: vec![i] })
                .collect()
        }
        Hq::Or(clauses) => clauses.iter().flat_map(|c| spans(c, toks, field_ok)).collect(),
        Hq::Near { clauses, slop, ordered, .. } => {
            let parts: Vec<Vec<Span>> = clauses.iter().map(|c| spans(c, toks, field_ok)).collect();
            if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
                return Vec::new();
            }
            let mut found = Vec::new();
            let mut chosen: Vec<&Span> = Vec::new();
            near(&parts, *slop, *ordered, &mut chosen, &mut found);
            found
        }
        Hq::First { inner, end } => {
            spans(inner, toks, field_ok).into_iter().filter(|s| s.end <= *end).collect()
        }
        Hq::Not { include, exclude, pre, post } => {
            let excluded = spans(exclude, toks, field_ok);
            spans(include, toks, field_ok)
                .into_iter()
                .filter(|s| {
                    !excluded.iter().any(|x| x.end + pre > s.start && x.start < s.end + post)
                })
                .collect()
        }
        // a containing or a within query marks the words of both of its
        // sides wherever one stands inside the other
        Hq::Containing { big, little } | Hq::Within { big, little } => {
            let small = spans(little, toks, field_ok);
            let within = matches!(q, Hq::Within { .. });
            spans(big, toks, field_ok)
                .into_iter()
                .filter_map(|b| {
                    let inside: Vec<&Span> =
                        small.iter().filter(|l| b.start <= l.start && l.end <= b.end).collect();
                    if inside.is_empty() {
                        return None;
                    }
                    let mut tokens = b.tokens.clone();
                    tokens.extend(inside.iter().flat_map(|l| l.tokens.iter().copied()));
                    tokens.sort_unstable();
                    tokens.dedup();
                    let (start, end) =
                        if within { (inside[0].start, inside[0].end) } else { (b.start, b.end) };
                    Some(Span { start, end, tokens })
                })
                .collect()
        }
        Hq::Loose(inner) => inner.iter().flat_map(|c| spans(c, toks, field_ok)).collect(),
    };
    out.sort_by_key(|s| (s.start, s.end));
    out.truncate(MAX_SPANS);
    out
}

/// Every way of taking one span from each clause that stands within `slop`.
fn near<'a>(
    parts: &'a [Vec<Span>],
    slop: usize,
    ordered: bool,
    chosen: &mut Vec<&'a Span>,
    found: &mut Vec<Span>,
) {
    if found.len() >= MAX_SPANS {
        return;
    }
    let k = chosen.len();
    if k == parts.len() {
        let start = chosen.iter().map(|s| s.start).min().unwrap_or(0);
        let end = chosen.iter().map(|s| s.end).max().unwrap_or(0);
        let length: usize = chosen.iter().map(|s| s.end - s.start).sum();
        if end.saturating_sub(start).saturating_sub(length) <= slop {
            let mut tokens: Vec<usize> = chosen.iter().flat_map(|s| s.tokens.clone()).collect();
            tokens.sort_unstable();
            tokens.dedup();
            found.push(Span { start, end, tokens });
        }
        return;
    }
    for span in &parts[k] {
        if ordered {
            if let Some(last) = chosen.last() {
                if span.start < last.end {
                    continue;
                }
                // the words only move further apart from here
                if span.start - chosen[0].start > slop + parts.len() * 8 + 64 {
                    break;
                }
            }
        } else if chosen.iter().any(|c| c.tokens.iter().any(|t| span.tokens.contains(t))) {
            continue;
        }
        if !ordered && !chosen.is_empty() {
            let start = chosen.iter().map(|s| s.start).min().unwrap_or(0).min(span.start);
            let end = chosen.iter().map(|s| s.end).max().unwrap_or(0).max(span.end);
            let length: usize =
                chosen.iter().map(|s| s.end - s.start).sum::<usize>() + span.end - span.start;
            if end - start > slop + length {
                continue;
            }
        }
        chosen.push(span);
        near(parts, slop, ordered, chosen, found);
        chosen.pop();
    }
}

fn boost_of(v: &Value) -> f32 {
    v.get("boost").and_then(|b| b.as_f64().or_else(|| b.as_str()?.parse().ok())).unwrap_or(1.0)
        as f32
}

/// The field a single-field query names, and its body.
fn field_body(body: &Value) -> Option<(&str, &Value)> {
    let o = body.as_object()?;
    o.iter()
        .find(|(k, _)| !matches!(k.as_str(), "boost" | "_name" | "rewrite" | "case_insensitive"))
        .map(|(k, v)| (k.as_str(), v))
}

/// The text of a field query given short (`{"f": "text"}`) or long
/// (`{"f": {"query": "text"}}`).
fn text_of(spec: &Value, key: &str) -> Option<String> {
    match spec {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Object(o) => o.get(key).and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }),
        _ => None,
    }
}

fn glob_regex(pattern: &str, insensitive: bool) -> Option<regex::Regex> {
    let mut s = String::from(if insensitive { "(?is)^" } else { "(?s)^" });
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '*' => s.push_str(".*"),
            '?' => s.push('.'),
            '\\' => {
                if let Some(next) = chars.next() {
                    s.push_str(&regex::escape(&next.to_string()));
                }
            }
            c => s.push_str(&regex::escape(&c.to_string())),
        }
    }
    s.push('$');
    regex::Regex::new(&s).ok()
}

fn lucene_regex(pattern: &str, insensitive: bool) -> Option<regex::Regex> {
    // Lucene's regexp is anchored at both ends and knows `<>` and `@` only
    // as flags; the rest reads the way a regex crate pattern does
    let flags = if insensitive { "(?is)" } else { "(?s)" };
    regex::Regex::new(&format!("{flags}^(?:{pattern})$")).ok()
}

/// Turns a query's JSON into what the highlighters read.
pub(super) struct Extract<'a> {
    pub(super) env: &'a Env<'a>,
}

impl Extract<'_> {
    pub(super) fn run(&self, query: Option<&Value>) -> Vec<Hq> {
        let mut out = Vec::new();
        if let Some(q) = query {
            self.walk(q, 1.0, &mut out);
        }
        out
    }

    fn walk(&self, node: &Value, boost: f32, out: &mut Vec<Hq>) {
        let Some(o) = node.as_object() else {
            if let Value::Array(a) = node {
                a.iter().for_each(|v| self.walk(v, boost, out));
            }
            return;
        };
        for (kind, body) in o {
            let b = boost * boost_of(body);
            match kind.as_str() {
                "bool" => {
                    // what a document must not match is not what it matched
                    for key in ["must", "should", "filter"] {
                        if let Some(c) = body.get(key) {
                            self.walk(c, b, out);
                        }
                    }
                }
                "dis_max" => {
                    if let Some(c) = body.get("queries") {
                        self.walk(c, b, out);
                    }
                }
                "constant_score" => {
                    if let Some(c) = body.get("filter") {
                        self.walk(c, b, out);
                    }
                }
                "function_score" | "script_score" | "nested" => {
                    if let Some(c) = body.get("query") {
                        self.walk(c, b, out);
                    }
                }
                "boosting" => {
                    if let Some(c) = body.get("positive") {
                        self.walk(c, b, out);
                    }
                }
                "hybrid" => {
                    if let Some(c) = body.get("queries") {
                        self.walk(c, b, out);
                    }
                }
                "has_child" | "has_parent" | "percolate" | "more_like_this" | "must_not" => {}
                "match" => self.match_query(body, boost, out),
                "match_phrase" => self.phrase_query(body, boost, false, out),
                "match_phrase_prefix" => self.phrase_query(body, boost, true, out),
                "match_bool_prefix" => {
                    if let Some((field, spec)) = field_body(body) {
                        let b = boost * boost_of(spec);
                        let analyzer = spec.get("analyzer").and_then(|v| v.as_str());
                        if let Some(text) = text_of(spec, "query") {
                            self.bool_prefix(field, &text, analyzer, b, out);
                        }
                    }
                }
                "multi_match" => self.multi_match(body, b, out),
                "combined_fields" => {
                    let text = body.get("query").and_then(|v| v.as_str()).unwrap_or("");
                    for field in self.fields_of(body.get("fields")) {
                        self.terms(&field, text, None, b, out);
                    }
                }
                "term" => {
                    if let Some((field, spec)) = field_body(body)
                        && let Some(value) = text_of(spec, "value")
                    {
                        let b = boost * boost_of(spec);
                        let leaf = match spec.get("case_insensitive").and_then(|v| v.as_bool()) {
                            Some(true) => {
                                glob_regex(&value.replace('*', "\\*").replace('?', "\\?"), true)
                                    .map(|re| Leaf::Pattern(re, value.clone()))
                                    .unwrap_or(Leaf::Term(value))
                            }
                            _ => Leaf::Term(self.env.normalized(field, &value)),
                        };
                        out.push(Hq::Leaf { field: field.to_string(), leaf, boost: b });
                    }
                }
                "terms" => {
                    if let Some(o) = body.as_object() {
                        for (field, values) in o {
                            let Some(values) = values.as_array() else { continue };
                            for v in values {
                                if let Some(t) = text_of(v, "value") {
                                    out.push(Hq::Leaf {
                                        field: field.clone(),
                                        leaf: Leaf::Term(self.env.normalized(field, &t)),
                                        boost: b,
                                    });
                                }
                            }
                        }
                    }
                }
                "prefix" | "wildcard" | "regexp" | "fuzzy" => {
                    if let Some(hq) = self.multi_term(kind, body, boost) {
                        out.push(hq);
                    }
                }
                "query_string" | "simple_query_string" => self.query_string(body, b, out),
                "intervals" => {
                    // An interval query reaches the highlighters as the terms
                    // it holds: the reference marks each of them wherever it
                    // stands, not only inside the interval that matched.
                    // The fast vector highlighter does not read them at all.
                    if let Some((field, rule)) = field_body(body) {
                        let mut terms = Vec::new();
                        self.interval_terms(field, rule, &mut terms);
                        out.push(Hq::Loose(terms));
                    }
                }
                k if k.starts_with("span_") || k == "field_masking_span" => {
                    if let Some(hq) = self.span(kind, body) {
                        out.push(hq.boosted(boost));
                    }
                }
                _ => {}
            }
        }
    }

    /// The fields a multi-field query lists, a pattern among them read
    /// against the mapping.
    fn fields_of(&self, fields: Option<&Value>) -> Vec<String> {
        let mut out = Vec::new();
        match fields.and_then(|f| f.as_array()) {
            Some(list) => {
                for f in list.iter().filter_map(|f| f.as_str()) {
                    let name = f.split('^').next().unwrap_or(f);
                    if name.contains('*') {
                        out.extend(self.env.fields_matching(name));
                    } else {
                        out.push(name.to_string());
                    }
                }
            }
            None => out.extend(self.env.fields_matching("*")),
        }
        out
    }

    fn match_query(&self, body: &Value, boost: f32, out: &mut Vec<Hq>) {
        let Some((field, spec)) = field_body(body) else { return };
        let b = boost * boost_of(spec);
        let analyzer = spec.get("analyzer").and_then(|v| v.as_str());
        let Some(text) = text_of(spec, "query") else { return };
        match spec.get("type").and_then(|t| t.as_str()) {
            Some("phrase") => self.phrase(field, &text, analyzer, 0, false, b, out),
            Some("phrase_prefix") => self.phrase(field, &text, analyzer, 0, true, b, out),
            _ => {
                let fuzzy = spec.get("fuzziness");
                match fuzzy {
                    Some(f)
                        if !matches!(f, Value::Number(n) if n.as_u64() == Some(0)) && f != "0" =>
                    {
                        let prefix = spec.get("prefix_length").and_then(|v| v.as_u64()).unwrap_or(0)
                            as usize;
                        for (term, _, _) in self.env.query_tokens(field, &text, analyzer) {
                            let edits =
                                crate::query::fuzzy_edits(Some(f), &term).unwrap_or(0) as usize;
                            let leaf = if edits == 0 {
                                Leaf::Term(term)
                            } else {
                                Leaf::Fuzzy { term, edits, prefix }
                            };
                            out.push(Hq::Leaf { field: field.to_string(), leaf, boost: b });
                        }
                    }
                    _ => self.terms(field, &text, analyzer, b, out),
                }
            }
        }
    }

    fn terms(
        &self,
        field: &str,
        text: &str,
        analyzer: Option<&str>,
        boost: f32,
        out: &mut Vec<Hq>,
    ) {
        for (term, _, _) in self.env.query_tokens(field, text, analyzer) {
            out.push(Hq::Leaf { field: field.to_string(), leaf: Leaf::Term(term), boost });
        }
    }

    fn phrase_query(&self, body: &Value, boost: f32, prefix: bool, out: &mut Vec<Hq>) {
        let Some((field, spec)) = field_body(body) else { return };
        let b = boost * boost_of(spec);
        let analyzer = spec.get("analyzer").and_then(|v| v.as_str());
        let slop = spec.get("slop").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        if let Some(text) = text_of(spec, "query") {
            self.phrase(field, &text, analyzer, slop, prefix, b, out);
        }
    }

    /// A phrase, as the highlighters rewrite one: a `span_near` of its words,
    /// in order when it allows no slop, and with room for the places the
    /// analyzer left empty (a stop word taken out) added to the slop.
    #[allow(clippy::too_many_arguments)]
    fn phrase(
        &self,
        field: &str,
        text: &str,
        analyzer: Option<&str>,
        slop: usize,
        prefix: bool,
        boost: f32,
        out: &mut Vec<Hq>,
    ) {
        let tokens = self.env.query_tokens(field, text, analyzer);
        if tokens.is_empty() {
            return;
        }
        // the words by the place they stand in
        let mut slots: Vec<(usize, Vec<String>)> = Vec::new();
        for (term, pos, _) in tokens {
            match slots.iter_mut().find(|(p, _)| *p == pos) {
                Some((_, terms)) => terms.push(term),
                None => slots.push((pos, vec![term])),
            }
        }
        slots.sort_by_key(|(p, _)| *p);
        let last = slots.len() - 1;
        let clause = |i: usize, terms: &Vec<String>| -> Hq {
            let leaves: Vec<Hq> = terms
                .iter()
                .map(|t| Hq::Leaf {
                    field: field.to_string(),
                    leaf: if prefix && i == last {
                        Leaf::Prefix(t.clone())
                    } else {
                        Leaf::Term(t.clone())
                    },
                    boost,
                })
                .collect();
            if leaves.len() == 1 {
                leaves.into_iter().next().unwrap_or(Hq::Or(Vec::new()))
            } else {
                Hq::Or(leaves)
            }
        };
        if slots.len() == 1 {
            // one word is a term query, and a term matches wherever it stands
            match clause(0, &slots[0].1) {
                Hq::Or(leaves) => out.extend(leaves),
                one => out.push(one),
            }
            return;
        }
        let gaps = (slots[last].0 - slots[0].0 + 1).saturating_sub(slots.len());
        let clauses: Vec<Hq> = slots.iter().enumerate().map(|(i, (_, t))| clause(i, t)).collect();
        // a derived field is searched by a query of its own that runs the
        // script, which the highlighters do not look inside: they get its
        // terms, and mark them wherever they stand
        if self.env.is_derived(field) {
            out.push(Hq::Loose(clauses));
            return;
        }
        out.push(Hq::Near { clauses, slop: slop + gaps, ordered: slop == 0, phrase: true, boost });
    }

    /// Every word a term, and the last the beginning of a word -- unless the
    /// field answers word beginnings from a field of its own, and then
    /// nothing in this field stands for it.
    fn bool_prefix(
        &self,
        field: &str,
        text: &str,
        analyzer: Option<&str>,
        boost: f32,
        out: &mut Vec<Hq>,
    ) {
        let own_prefixes = self.env.answers_prefixes_elsewhere(field);
        if self.env.shingle_width(field).is_some() {
            let words: Vec<&str> = text.split_whitespace().collect();
            if words.len() > 1 {
                self.terms(field, &words[..words.len() - 1].join(" "), analyzer, boost, out);
            }
            return;
        }
        let tokens = self.env.query_tokens(field, text, analyzer);
        let n = tokens.len();
        for (i, (term, _, _)) in tokens.into_iter().enumerate() {
            let leaf = if i + 1 == n {
                if own_prefixes {
                    continue;
                }
                Leaf::Prefix(term)
            } else {
                Leaf::Term(term)
            };
            out.push(Hq::Leaf { field: field.to_string(), leaf, boost });
        }
    }

    fn multi_match(&self, body: &Value, boost: f32, out: &mut Vec<Hq>) {
        let text = match body.get("query") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => return,
        };
        let analyzer = body.get("analyzer").and_then(|v| v.as_str());
        let slop = body.get("slop").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        for field in self.fields_of(body.get("fields")) {
            match body.get("type").and_then(|t| t.as_str()) {
                Some("phrase") => self.phrase(&field, &text, analyzer, slop, false, boost, out),
                Some("phrase_prefix") => {
                    self.phrase(&field, &text, analyzer, slop, true, boost, out)
                }
                Some("bool_prefix") => self.bool_prefix(&field, &text, analyzer, boost, out),
                _ => self.terms(&field, &text, analyzer, boost, out),
            }
        }
    }

    fn multi_term(&self, kind: &str, body: &Value, boost: f32) -> Option<Hq> {
        let (field, spec) = field_body(body)?;
        let b = boost * boost_of(spec);
        let insensitive = spec.get("case_insensitive").and_then(|v| v.as_bool()).unwrap_or(false);
        let value = text_of(spec, "value").or_else(|| text_of(spec, "wildcard"))?;
        let leaf = match kind {
            "prefix" => {
                // a field that keeps word beginnings of its own answers a
                // short prefix from them, and the field itself is not asked
                if self.env.prefix_answered_elsewhere(field, &value) {
                    return None;
                }
                if insensitive {
                    Leaf::Pattern(
                        glob_regex(
                            &format!("{}*", value.replace('*', "\\*").replace('?', "\\?")),
                            true,
                        )?,
                        value,
                    )
                } else {
                    Leaf::Prefix(value)
                }
            }
            "wildcard" => Leaf::Pattern(glob_regex(&value, insensitive)?, value),
            "regexp" => Leaf::Pattern(lucene_regex(&value, insensitive)?, value),
            _ => {
                let edits = crate::query::fuzzy_edits(
                    Some(spec.get("fuzziness").unwrap_or(&Value::String("AUTO".into()))),
                    &value,
                )
                .unwrap_or(0) as usize;
                let prefix =
                    spec.get("prefix_length").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                Leaf::Fuzzy { term: value, edits, prefix }
            }
        };
        Some(Hq::Leaf { field: field.to_string(), leaf, boost: b })
    }

    fn interval_terms(&self, field: &str, rule: &Value, out: &mut Vec<Hq>) {
        let Some(o) = rule.as_object() else { return };
        for (kind, body) in o {
            match kind.as_str() {
                "match" => {
                    let text = body.get("query").and_then(|v| v.as_str()).unwrap_or("");
                    let target = body.get("use_field").and_then(|v| v.as_str()).unwrap_or(field);
                    let analyzer = body.get("analyzer").and_then(|v| v.as_str());
                    self.terms(target, text, analyzer, 1.0, out);
                }
                "any_of" | "all_of" => {
                    for sub in
                        body.get("intervals").and_then(|v| v.as_array()).into_iter().flatten()
                    {
                        self.interval_terms(field, sub, out);
                    }
                }
                "prefix" | "wildcard" | "fuzzy" => {
                    let key = if kind == "prefix" {
                        "prefix"
                    } else if kind == "wildcard" {
                        "pattern"
                    } else {
                        "term"
                    };
                    let Some(value) = body.get(key).and_then(|v| v.as_str()) else { continue };
                    let target = body.get("use_field").and_then(|v| v.as_str()).unwrap_or(field);
                    let leaf = match kind.as_str() {
                        "prefix" => Leaf::Prefix(self.env.normalized(target, value)),
                        "wildcard" => match glob_regex(value, false) {
                            Some(re) => Leaf::Pattern(re, value.to_string()),
                            None => continue,
                        },
                        _ => Leaf::Fuzzy {
                            term: value.to_string(),
                            edits: crate::query::fuzzy_edits(
                                body.get("fuzziness").or(Some(&Value::String("AUTO".into()))),
                                value,
                            )
                            .unwrap_or(0) as usize,
                            prefix: body.get("prefix_length").and_then(|v| v.as_u64()).unwrap_or(0)
                                as usize,
                        },
                    };
                    out.push(Hq::Leaf { field: target.to_string(), leaf, boost: 1.0 });
                }
                _ => {}
            }
        }
    }

    fn span(&self, kind: &str, body: &Value) -> Option<Hq> {
        match kind {
            "span_term" => {
                let (field, spec) = field_body(body)?;
                let value = text_of(spec, "value").or_else(|| text_of(spec, "term"))?;
                Some(Hq::Leaf {
                    field: field.to_string(),
                    leaf: Leaf::Term(value),
                    boost: boost_of(spec),
                })
            }
            "span_near" => {
                let clauses: Vec<Hq> = body
                    .get("clauses")?
                    .as_array()?
                    .iter()
                    .filter_map(|c| self.span_clause(c))
                    .collect();
                let slop = body.get("slop").and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize;
                let ordered = body.get("in_order").and_then(|v| v.as_bool()).unwrap_or(true);
                Some(Hq::Near { clauses, slop, ordered, phrase: false, boost: boost_of(body) })
            }
            "span_or" => {
                let clauses: Vec<Hq> = body
                    .get("clauses")?
                    .as_array()?
                    .iter()
                    .filter_map(|c| self.span_clause(c))
                    .collect();
                Some(Hq::Or(clauses))
            }
            "span_first" => {
                let inner = self.span_clause(body.get("match")?)?;
                let end = body.get("end").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                Some(Hq::First { inner: Box::new(inner), end })
            }
            "span_not" => {
                let include = self.span_clause(body.get("include")?)?;
                let exclude = self.span_clause(body.get("exclude")?)?;
                let dist = body.get("dist").and_then(|v| v.as_u64());
                let pre = body.get("pre").and_then(|v| v.as_u64()).or(dist).unwrap_or(0) as usize;
                let post = body.get("post").and_then(|v| v.as_u64()).or(dist).unwrap_or(0) as usize;
                Some(Hq::Not { include: Box::new(include), exclude: Box::new(exclude), pre, post })
            }
            "span_containing" | "span_within" => {
                let big = Box::new(self.span_clause(body.get("big")?)?);
                let little = Box::new(self.span_clause(body.get("little")?)?);
                Some(if kind == "span_containing" {
                    Hq::Containing { big, little }
                } else {
                    Hq::Within { big, little }
                })
            }
            "span_multi" => {
                let inner = body.get("match")?.as_object()?;
                let (k, b) = inner.iter().next()?;
                self.multi_term(k, b, 1.0)
            }
            "field_masking_span" => self.span_clause(body.get("query")?),
            _ => None,
        }
    }

    fn span_clause(&self, node: &Value) -> Option<Hq> {
        let (k, b) = node.as_object()?.iter().next()?;
        self.span(k, b)
    }

    /// A query string as far as highlighting reads it: its words and
    /// phrases, each asked of the fields it names or of the default ones,
    /// and nothing that stands behind a `-` or a `NOT`.
    fn query_string(&self, body: &Value, boost: f32, out: &mut Vec<Hq>) {
        let text = body.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let mut defaults: Vec<String> = Vec::new();
        if let Some(f) = body.get("default_field").and_then(|v| v.as_str()) {
            if f.contains('*') {
                defaults.extend(self.env.fields_matching(f));
            } else {
                defaults.push(f.to_string());
            }
        }
        if body.get("fields").is_some() {
            defaults.extend(self.fields_of(body.get("fields")));
        }
        if body.get("default_field").is_none() && body.get("fields").is_none() {
            defaults.extend(self.env.fields_matching("*"));
        }
        let analyzer = body.get("analyzer").and_then(|v| v.as_str());
        let mut negate = false;
        for raw in split_query_string(text) {
            let upper = raw.to_ascii_uppercase();
            match upper.as_str() {
                "AND" | "&&" | "OR" | "||" | "|" | "+" => continue,
                "NOT" | "!" => {
                    negate = true;
                    continue;
                }
                _ => {}
            }
            let mut tok = raw.as_str();
            if let Some(rest) = tok.strip_prefix('-').or_else(|| tok.strip_prefix('!')) {
                negate = true;
                tok = rest;
            }
            tok = tok.strip_prefix('+').unwrap_or(tok);
            if std::mem::take(&mut negate) || tok.is_empty() {
                continue;
            }
            let (fields, value) = match tok.split_once(':') {
                Some((f, v)) if !f.is_empty() && !f.starts_with('"') && !f.starts_with('(') => {
                    let named = f.trim_start_matches('(');
                    let fields = if named.contains('*') {
                        self.env.fields_matching(named)
                    } else {
                        vec![named.to_string()]
                    };
                    (fields, v)
                }
                _ => (defaults.clone(), tok),
            };
            let value = value.trim_start_matches('(').trim_end_matches(')');
            // a boost written after the word is not part of it
            let value = match value.rsplit_once('^') {
                Some((v, b)) if b.parse::<f32>().is_ok() => v,
                _ => value,
            };
            if value.is_empty() || value.starts_with('[') || value.starts_with('{') {
                continue;
            }
            for field in &fields {
                if let Some(inner) = value.strip_prefix('"') {
                    let (inner, slop) = match inner.rsplit_once('"') {
                        Some((i, rest)) => {
                            (i, rest.strip_prefix('~').and_then(|s| s.parse().ok()).unwrap_or(0))
                        }
                        None => (inner, 0),
                    };
                    self.phrase(field, inner, analyzer, slop, false, boost, out);
                } else if value.len() > 2 && value.starts_with('/') && value.ends_with('/') {
                    if let Some(re) = lucene_regex(&value[1..value.len() - 1], false) {
                        out.push(Hq::Leaf {
                            field: field.clone(),
                            leaf: Leaf::Pattern(re, value.to_string()),
                            boost,
                        });
                    }
                } else if let Some(stem) =
                    value.strip_suffix('*').filter(|s| !s.contains(['*', '?']))
                {
                    let stem = stem.to_lowercase();
                    if !self.env.prefix_answered_elsewhere(field, &stem) {
                        out.push(Hq::Leaf {
                            field: field.clone(),
                            leaf: Leaf::Prefix(stem),
                            boost,
                        });
                    }
                } else if value.contains(['*', '?']) {
                    if let Some(re) = glob_regex(&value.to_lowercase(), false) {
                        out.push(Hq::Leaf {
                            field: field.clone(),
                            leaf: Leaf::Pattern(re, value.to_string()),
                            boost,
                        });
                    }
                } else if let Some((word, fuzz)) = value.split_once('~') {
                    let edits = if fuzz.is_empty() { None } else { fuzz.parse::<u64>().ok() };
                    for (term, _, _) in self.env.query_tokens(field, word, analyzer) {
                        let edits = match edits {
                            Some(e) => e.min(2) as usize,
                            None => crate::query::fuzzy_edits(
                                Some(&Value::String("AUTO".into())),
                                &term,
                            )
                            .unwrap_or(0) as usize,
                        };
                        out.push(Hq::Leaf {
                            field: field.clone(),
                            leaf: Leaf::Fuzzy { term, edits, prefix: 0 },
                            boost,
                        });
                    }
                } else {
                    self.terms(field, value, analyzer, boost, out);
                }
            }
        }
    }
}

/// Cut a query string at its spaces, a quoted phrase kept whole.
fn split_query_string(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            '(' | ')' if !quoted => {
                if !cur.is_empty() && c == '(' && !cur.ends_with(':') {
                    out.push(std::mem::take(&mut cur));
                }
                if c == ')' && !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
