//! OpenSearch query DSL -> BoostCore queries.

use crate::store::{Fields, Mapping};
use anyhow::{Result, anyhow};
use boostcore::query::{
    AllQuery, AutomatonWeight, BooleanQuery, BoostQuery, EmptyQuery, EnableScoring, ExistsQuery,
    FuzzyTermQuery, Occur, PhraseQuery, Query, RangeQuery, TermQuery, Weight,
};
use boostcore::schema::{Field, IndexRecordOption, Term, Type};
use boostcore::{Index, TantivyError};
use boostcore_fst::Regex;
use serde_json::Value;
use std::ops::Bound;
use std::sync::Arc;

mod dispatch;
pub(crate) use dispatch::*;

mod analyze;
pub(crate) use analyze::*;
mod bool;
pub(crate) use bool::*;
mod range;
pub(crate) use range::*;
mod pattern;
pub(crate) use pattern::*;
mod terms;
pub(crate) use terms::*;
mod text;
pub(crate) use text::*;

pub struct Ctx<'a> {
    pub fields: &'a Fields,
    pub mapping: &'a Mapping,
    pub index: &'a Index,
    pub max_terms_count: usize,
    pub max_regex_length: usize,
    /// whether the cluster still allows the queries that cost the most to run
    pub allow_expensive: bool,
    /// value kinds seen per field path, used to narrow typed range variants
    pub observed_kinds: &'a std::collections::HashMap<String, u8>,
    pub kinds_complete: bool,
    pub stats: &'a std::sync::Arc<crate::blockstats::StatsCache>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum View {
    Dyn,
    Raw,
}

impl<'a> Ctx<'a> {
    pub fn field_of(&self, v: View) -> Field {
        match v {
            View::Dyn => self.fields.dynamic,
            View::Raw => self.fields.raw,
        }
    }

    /// Which of the two JSON views backs this field name.
    ///
    /// `analyzed` is true for full-text contexts (`match`), false for exact ones
    /// (`term`, sorting, term aggregations) -- mirroring the text/keyword split.
    pub fn view(&self, field: &str, analyzed: bool) -> View {
        match self.mapping.type_of(field) {
            Some("text") | Some("match_only_text") | Some("search_as_you_type") => View::Dyn,
            Some("keyword")
            | Some("constant_keyword")
            | Some("wildcard")
            | Some("ip")
            | Some("flat_object") => View::Raw,
            Some(_) => View::Dyn, // numeric, date, boolean: identical in both views
            None => {
                // a path inside a flat_object is exact, like a keyword; the
                // mapping never names it, so the ancestor has to be consulted
                let mut prefix = field;
                while let Some((head, _)) = prefix.rsplit_once('.') {
                    if self.mapping.type_of(head) == Some("flat_object") {
                        return View::Raw;
                    }
                    prefix = head;
                }
                if analyzed { View::Dyn } else { View::Raw }
            }
        }
    }

    /// `title.keyword` addresses the raw view of `title`.
    pub fn resolve(&self, field: &str, analyzed: bool) -> (Field, String, View) {
        // a field declared as an alias is another name for one that is really
        // there, and a query asking by that name asks about the real one
        if let Some(real) = self.mapping.target_of(field) {
            let real = real.to_string();
            return self.resolve(&real, analyzed);
        }
        // naming a flat_object itself asks about every value beneath it
        if self.mapping.type_of(field) == Some("flat_object") {
            let path = format!("{field}.{}", crate::store::FLAT_VALUES);
            let v = self.view(field, analyzed);
            return (self.field_of(v), path, v);
        }
        // `.keyword` is how a text field's untouched view is addressed, and a
        // mapping that does not declare the sub-field does not change that
        if self.mapping.type_of(field).is_none()
            && let Some(base) = field.strip_suffix(".keyword")
        {
            return (self.fields.raw, base.to_string(), View::Raw);
        }
        let v = self.view(field, analyzed);
        (self.field_of(v), field.to_string(), v)
    }

    pub fn column_name(&self, field: &str, analyzed: bool) -> String {
        let (_, path, view) = self.resolve(field, analyzed);
        let prefix = if view == View::Raw { crate::store::RAW } else { crate::store::DYN };
        format!("{prefix}.{path}")
    }
}

/// A regex/automaton query scoped to one JSON path.
struct JsonAutomatonQuery {
    field: Field,
    regex: Arc<Regex>,
    json_path_bytes: Vec<u8>,
}

impl std::fmt::Debug for JsonAutomatonQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JsonAutomatonQuery({:?})", self.field)
    }
}

impl Clone for JsonAutomatonQuery {
    fn clone(&self) -> Self {
        JsonAutomatonQuery {
            field: self.field,
            regex: self.regex.clone(),
            json_path_bytes: self.json_path_bytes.clone(),
        }
    }
}

impl Query for JsonAutomatonQuery {
    fn weight(&self, _s: EnableScoring<'_>) -> boostcore::Result<Box<dyn Weight>> {
        Ok(Box::new(AutomatonWeight::<Regex>::new_for_json_path(
            self.field,
            self.regex.clone(),
            &self.json_path_bytes,
        )))
    }
}

fn single_key(o: &Value) -> Result<(String, Value)> {
    let obj = o.as_object().ok_or_else(|| anyhow!("expected object"))?;
    let (k, v) = obj.iter().next().ok_or_else(|| anyhow!("empty query clause"))?;
    Ok((k.clone(), v.clone()))
}

/// Extract `{"field": value}` or `{"field": {"value": v, ...}}`.
/// The suite writes flags both as JSON booleans and as the strings the URL form
/// would carry.
fn is_true(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

fn field_and_value(v: &Value) -> Result<(String, Value, Value)> {
    let (field, body) = single_key(v)?;
    if let Some(o) = body.as_object()
        && let Some(val) = o.get("value").or_else(|| o.get("query"))
    {
        return Ok((field, val.clone(), body.clone()));
    }
    Ok((field, body.clone(), Value::Null))
}

/// A pragmatic `query_string` subset: `field:term`, quoted phrases, wildcards,
/// AND/OR/NOT, and `default_field` / `default_operator`.
fn build_query_string(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
    let text = body.get("query").and_then(|v| v.as_str()).unwrap_or_default();
    // a whole query written between slashes is one regex, whatever whitespace
    // it happens to contain, and it costs what any other pattern costs
    let whole = text.trim();
    if whole.len() > 2 && whole.starts_with('/') && whole.ends_with('/') {
        let n = whole.chars().count() - 2;
        if n > ctx.max_regex_length {
            return Err(anyhow!(
                "The length of regex [{n}] used in the Regexp Query request has exceeded the \
                 allowed maximum of [{}]. This maximum can be set by changing the \
                 [index.max_regex_length] index level setting.",
                ctx.max_regex_length
            ));
        }
    }
    let default_operator =
        body.get("default_operator").and_then(|v| v.as_str()).unwrap_or("or").to_ascii_lowercase();
    let mut default_fields: Vec<String> = Vec::new();
    if let Some(f) = body.get("default_field").and_then(|v| v.as_str()) {
        default_fields.push(f.to_string());
    }
    if let Some(arr) = body.get("fields").and_then(|v| v.as_array()) {
        for f in arr {
            match f.as_str() {
                Some(s) => default_fields.push(s.split('^').next().unwrap_or(s).to_string()),
                None => {
                    return Err(anyhow!("[query_string] field name in [fields] cannot be null"));
                }
            }
        }
    }
    if default_fields.is_empty() {
        default_fields = ctx.mapping.types.keys().cloned().collect();
        default_fields.sort();
    }

    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    let mut should_count = 0usize;
    let mut pending_not = false;
    let mut pending_occur: Option<Occur> = None;

    for tok in split_query_string(text) {
        match tok.to_ascii_uppercase().as_str() {
            "AND" | "&&" => {
                pending_occur = Some(Occur::Must);
                continue;
            }
            "OR" | "||" => {
                pending_occur = Some(Occur::Should);
                continue;
            }
            "NOT" | "!" => {
                pending_not = true;
                continue;
            }
            _ => {}
        }
        let (field_part, value) = match tok.split_once(':') {
            Some((f, v)) if !f.is_empty() && !f.contains(' ') => {
                (Some(f.to_string()), v.to_string())
            }
            _ => (None, tok.clone()),
        };
        let targets: Vec<String> =
            field_part.map(|f| vec![f]).unwrap_or_else(|| default_fields.clone());
        let value = if value.starts_with('[') || value.starts_with('{') {
            value
        } else {
            value.trim_matches('"').to_string()
        };
        if value.is_empty() {
            continue;
        }
        let regex_literal = value.len() > 2 && value.starts_with('/') && value.ends_with('/');
        // a pattern written between slashes is a regex, and costs the same as
        // one asked for by name
        if regex_literal {
            let n = value.chars().count() - 2;
            if n > ctx.max_regex_length {
                return Err(anyhow!(
                    "The length of regex [{n}] used in the Regexp Query request has exceeded the \
                     allowed maximum of [{}]. This maximum can be set by changing the \
                     [index.max_regex_length] index level setting.",
                    ctx.max_regex_length
                ));
            }
        }
        // `field:[a TO b]` is a range, not a term
        if let Some(spec) = parse_range_token(&value) {
            let mut per_field: Vec<Box<dyn Query>> = Vec::new();
            for name in &targets {
                let clause = serde_json::json!({"range": { name.clone(): spec.clone() }});
                if let Ok(q) = build(ctx, &clause) {
                    per_field.push(q);
                }
            }
            if !per_field.is_empty() {
                let mut per_field = per_field;
                let sub: Box<dyn Query> = match per_field.len() {
                    1 => per_field.remove(0),
                    _ => Box::new(BooleanQuery::union(per_field)),
                };
                let occur = if pending_not {
                    Occur::MustNot
                } else {
                    pending_occur.take().unwrap_or(if default_operator == "and" {
                        Occur::Must
                    } else {
                        Occur::Should
                    })
                };
                pending_not = false;
                if occur == Occur::Should {
                    should_count += 1;
                }
                clauses.push((occur, sub));
            }
            continue;
        }
        let mut per_field: Vec<Box<dyn Query>> = Vec::new();
        for name in &targets {
            let (f, path, view) = ctx.resolve(name, true);
            if regex_literal {
                // the analysed view holds lowercased terms, so a pattern
                // written in capitals has to be lowered to meet them
                let pat = if view == View::Dyn {
                    lowercase_regex(&value[1..value.len() - 1])
                } else {
                    value[1..value.len() - 1].to_string()
                };
                if let Ok(q) = regex_query(f, &path, &pat) {
                    per_field.push(q);
                }
                continue;
            }
            if value.contains('*') || value.contains('?') {
                let pat = if view == View::Dyn { value.to_lowercase() } else { value.clone() };
                if let Ok(q) = regex_query(f, &path, &wildcard_to_regex(&pat)) {
                    per_field.push(q);
                }
                continue;
            }
            let clause = serde_json::json!({ name.clone(): value.clone() });
            if let Ok(q) = build_match(ctx, "match", &clause) {
                per_field.push(q);
            }
        }
        if per_field.is_empty() {
            continue;
        }
        let mut per_field = per_field;
        let sub: Box<dyn Query> = match per_field.len() {
            1 => per_field.remove(0),
            _ => Box::new(BooleanQuery::union(per_field)),
        };
        let occur = if pending_not {
            Occur::MustNot
        } else {
            pending_occur.take().unwrap_or(if default_operator == "and" {
                Occur::Must
            } else {
                Occur::Should
            })
        };
        pending_not = false;
        if occur == Occur::Should {
            should_count += 1;
        }
        clauses.push((occur, sub));
    }

    if clauses.is_empty() {
        return Ok(Box::new(EmptyQuery));
    }
    if clauses.iter().all(|(o, _)| *o == Occur::MustNot) {
        clauses.push((Occur::Must, Box::new(AllQuery)));
    }
    let required =
        if should_count > 0 && clauses.iter().all(|(o, _)| *o != Occur::Must) { 1 } else { 0 };
    Ok(Box::new(BooleanQuery::with_minimum_required_clauses(clauses, required)))
}

/// Split on whitespace, keeping quoted phrases and bracketed ranges together,
/// so `field:[3 TO 4]` survives as one token.
fn split_query_string(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            '[' | '{' if !in_quotes => {
                depth += 1;
                cur.push(c);
            }
            ']' | '}' if !in_quotes => {
                depth -= 1;
                cur.push(c);
            }
            c if c.is_whitespace() && !in_quotes && depth <= 0 => {
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

/// `[lo TO hi]` is inclusive, `{lo TO hi}` exclusive; `*` is an open end.
fn parse_range_token(value: &str) -> Option<Value> {
    let (open, close) = (value.chars().next()?, value.chars().last()?);
    let inclusive_lo = match open {
        '[' => true,
        '{' => false,
        _ => return None,
    };
    let inclusive_hi = match close {
        ']' => true,
        '}' => false,
        _ => return None,
    };
    let inner = &value[1..value.len() - 1];
    let mut parts = inner.splitn(2, " TO ");
    let lo = parts.next()?.trim();
    let hi = parts.next()?.trim();
    let as_json = |t: &str| -> Option<Value> {
        if t == "*" {
            return None;
        }
        Some(serde_json::from_str(t).unwrap_or_else(|_| Value::String(t.to_string())))
    };
    let mut spec = serde_json::Map::new();
    if let Some(v) = as_json(lo) {
        spec.insert(if inclusive_lo { "gte" } else { "gt" }.into(), v);
    }
    if let Some(v) = as_json(hi) {
        spec.insert(if inclusive_hi { "lte" } else { "lt" }.into(), v);
    }
    Some(Value::Object(spec))
}

/// A constant score over another query.
///
/// BoostCore has one of these already, but its weight leaves `for_each_pruning`
/// to the blanket implementation, which walks every matching document. That
/// throws away the block-skipping a term query would otherwise do, and a term
/// query is exactly what gets wrapped here.
///
/// A constant score makes pruning simpler than block-WAND, not harder: every
/// document scores the same, so once the collector's threshold has reached
/// that score its heap is full of ties and nothing later can displace them.
/// The walk stops there. Ties go to the lower document id either way, which
/// is what the blanket implementation would have arrived at the slow way.
pub struct ConstScore {
    query: Box<dyn Query>,
    score: boostcore::Score,
}

impl ConstScore {
    pub fn new(query: Box<dyn Query>, score: boostcore::Score) -> Self {
        ConstScore { query, score }
    }
}

impl std::fmt::Debug for ConstScore {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Const(score={}, query={:?})", self.score, self.query)
    }
}

impl Clone for ConstScore {
    fn clone(&self) -> Self {
        ConstScore { query: self.query.box_clone(), score: self.score }
    }
}

impl Query for ConstScore {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> boostcore::Result<Box<dyn Weight>> {
        let inner = self.query.weight(enable_scoring)?;
        // with scoring off the score is never read, so the wrapper is pure cost
        Ok(if enable_scoring.is_scoring_enabled() {
            Box::new(ConstWeight { inner, score: self.score })
        } else {
            inner
        })
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a Term, bool)) {
        self.query.query_terms(visitor);
    }
}

struct ConstWeight {
    inner: Box<dyn Weight>,
    score: boostcore::Score,
}

impl Weight for ConstWeight {
    fn scorer(
        &self,
        reader: &boostcore::SegmentReader,
        boost: boostcore::Score,
    ) -> boostcore::Result<Box<dyn boostcore::query::Scorer>> {
        let inner = self.inner.scorer(reader, boost)?;
        Ok(Box::new(boostcore::query::ConstScorer::new(inner, boost * self.score)))
    }

    fn explain(
        &self,
        reader: &boostcore::SegmentReader,
        doc: boostcore::DocId,
    ) -> boostcore::Result<boostcore::query::Explanation> {
        let mut ex = boostcore::query::Explanation::new("Const", self.score);
        ex.add_detail(self.inner.explain(reader, doc)?);
        Ok(ex)
    }

    fn count(&self, reader: &boostcore::SegmentReader) -> boostcore::Result<u32> {
        self.inner.count(reader)
    }

    fn for_each_pruning(
        &self,
        threshold: boostcore::Score,
        reader: &boostcore::SegmentReader,
        callback: &mut dyn FnMut(boostcore::DocId, boostcore::Score) -> boostcore::Score,
    ) -> boostcore::Result<()> {
        use boostcore::DocSet;
        // nothing here can beat what the collector already holds
        if threshold >= self.score {
            return Ok(());
        }
        // the inner scorer is walked for its documents alone; the score it
        // would compute is discarded, so ask for the cheaper unscored form
        let mut scorer = self.inner.scorer(reader, 1.0)?;
        let mut doc = scorer.doc();
        while doc != boostcore::TERMINATED {
            if callback(doc, self.score) >= self.score {
                return Ok(());
            }
            doc = scorer.advance();
        }
        Ok(())
    }
}

// ------------------------------------------------------- intervals, exactly
//
// The `intervals` query asks where in a field the words are, not only whether
// they are there. Positions are not kept in a column, so the text is read back
// and analysed again -- the same trade `significant_text` makes -- and the
// rules are then evaluated over the tokens.

/// One stretch of a field, given by the first and last token it covers.
type Span = (usize, usize);

/// Every stretch of the token list that satisfies a rule, one per starting
/// point, shortest first.
pub fn interval_spans(
    tokens: &[String],
    rule: &Value,
    analyse: &dyn Fn(&str) -> Vec<String>,
) -> Vec<Span> {
    let Some((kind, spec)) = rule.as_object().and_then(|o| o.iter().next()) else {
        return Vec::new();
    };
    let ordered = match spec.get("mode").and_then(|m| m.as_str()) {
        Some(m) => m.starts_with("ordered"),
        None => is_true(spec.get("ordered")),
    };
    let no_overlap = spec.get("mode").and_then(|m| m.as_str()) == Some("unordered_no_overlap");
    let max_gaps = spec.get("max_gaps").and_then(|v| v.as_i64());
    let mut spans = match kind.as_str() {
        "match" => {
            let text = spec.get("query").and_then(|v| v.as_str()).unwrap_or_default();
            let words = analyse(text);
            let places: Vec<Vec<usize>> = words
                .iter()
                .map(|w| {
                    tokens.iter().enumerate().filter(|(_, t)| *t == w).map(|(i, _)| i).collect()
                })
                .collect();
            combine_spans(
                &places
                    .iter()
                    .map(|p| p.iter().map(|i| (*i, *i)).collect())
                    .collect::<Vec<Vec<Span>>>(),
                ordered,
                no_overlap,
            )
        }
        "prefix" => {
            let want =
                spec.get("prefix").and_then(|v| v.as_str()).unwrap_or_default().to_lowercase();
            single_spans(tokens, &|t| t.starts_with(&want))
        }
        "wildcard" => {
            let pat = spec.get("pattern").and_then(|v| v.as_str()).unwrap_or_default();
            let re = regex::Regex::new(&format!("(?i)^{}$", wildcard_to_regex_source(pat)));
            match re {
                Ok(re) => single_spans(tokens, &|t| re.is_match(t)),
                Err(_) => Vec::new(),
            }
        }
        "regexp" => {
            let pat = spec.get("pattern").and_then(|v| v.as_str()).unwrap_or_default();
            let insensitive = is_true(spec.get("case_insensitive"));
            let head = if insensitive { "(?i)" } else { "" };
            match regex::Regex::new(&format!("{head}^{pat}$")) {
                Ok(re) => single_spans(tokens, &|t| re.is_match(t)),
                Err(_) => Vec::new(),
            }
        }
        "fuzzy" => {
            let want = spec.get("term").and_then(|v| v.as_str()).unwrap_or_default().to_lowercase();
            let edits = spec.get("fuzziness").and_then(|v| v.as_u64()).unwrap_or(2) as usize;
            single_spans(tokens, &|t| levenshtein_within(t, &want, edits))
        }
        "all_of" | "any_of" => {
            let children: Vec<Vec<Span>> = spec
                .get("intervals")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().map(|r| interval_spans(tokens, r, analyse)).collect())
                .unwrap_or_default();
            if kind == "any_of" {
                let mut all: Vec<Span> = children.into_iter().flatten().collect();
                all.sort();
                all.dedup();
                all
            } else {
                combine_spans(&children, ordered, no_overlap)
            }
        }
        _ => Vec::new(),
    };
    // `max_gaps` limits how much of the stretch is not the rule itself
    if let Some(gaps) = max_gaps.filter(|g| *g >= 0) {
        let width = |s: &Span| (s.1 - s.0 + 1) as i64;
        let terms = rule_width(rule, analyse);
        spans.retain(|s| width(s) - terms <= gaps);
    }
    if let Some(filter) = spec.get("filter").and_then(|f| f.as_object()) {
        for (name, inner) in filter {
            let other = interval_spans(tokens, inner, analyse);
            spans.retain(|s| match name.as_str() {
                "containing" => other.iter().any(|o| s.0 <= o.0 && o.1 <= s.1),
                "not_containing" => !other.iter().any(|o| s.0 <= o.0 && o.1 <= s.1),
                "contained_by" => other.iter().any(|o| o.0 <= s.0 && s.1 <= o.1),
                "not_contained_by" => !other.iter().any(|o| o.0 <= s.0 && s.1 <= o.1),
                "overlapping" => other.iter().any(|o| s.0 <= o.1 && o.0 <= s.1),
                "not_overlapping" => !other.iter().any(|o| s.0 <= o.1 && o.0 <= s.1),
                "before" => other.iter().any(|o| s.1 < o.0),
                "after" => other.iter().any(|o| o.1 < s.0),
                _ => true,
            });
        }
    }
    spans
}

/// How many tokens a rule is made of, which is what `max_gaps` counts against.
fn rule_width(rule: &Value, analyse: &dyn Fn(&str) -> Vec<String>) -> i64 {
    let Some((kind, spec)) = rule.as_object().and_then(|o| o.iter().next()) else { return 1 };
    match kind.as_str() {
        "match" => {
            analyse(spec.get("query").and_then(|v| v.as_str()).unwrap_or_default()).len() as i64
        }
        "all_of" => spec
            .get("intervals")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().map(|r| rule_width(r, analyse)).sum())
            .unwrap_or(1),
        "any_of" => spec
            .get("intervals")
            .and_then(|v| v.as_array())
            .and_then(|a| a.iter().map(|r| rule_width(r, analyse)).min())
            .unwrap_or(1),
        _ => 1,
    }
}

fn single_spans(tokens: &[String], hit: &dyn Fn(&str) -> bool) -> Vec<Span> {
    tokens.iter().enumerate().filter(|(_, t)| hit(t)).map(|(i, _)| (i, i)).collect()
}

/// The shortest stretch covering one span from each part, in order or not.
fn combine_spans(parts: &[Vec<Span>], ordered: bool, no_overlap: bool) -> Vec<Span> {
    if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return Vec::new();
    }
    if parts.len() == 1 {
        return parts[0].clone();
    }
    // where overlap is forbidden the parts are folded together two at a time,
    // so a stretch already built is what the next part must keep clear of
    if no_overlap && parts.len() > 2 {
        let mut acc = combine_spans(&parts[..2], ordered, no_overlap);
        for part in &parts[2..] {
            acc = combine_spans(&[acc, part.clone()], ordered, no_overlap);
        }
        return acc;
    }
    let mut out: Vec<Span> = Vec::new();
    if ordered {
        for first in &parts[0] {
            let mut at = *first;
            let mut ok = true;
            for part in &parts[1..] {
                match part.iter().filter(|s| s.0 > at.1).min_by_key(|s| s.1) {
                    Some(next) => at = *next,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                out.push((first.0, at.1));
            }
        }
    } else {
        // every part has to be somewhere; the stretch runs from the earliest
        // of the chosen spans to the latest, and where overlap is forbidden no
        // two parts may claim the same tokens
        let mut chosen: Vec<Span> = Vec::with_capacity(parts.len());
        fn walk(
            parts: &[Vec<Span>],
            at: usize,
            chosen: &mut Vec<Span>,
            no_overlap: bool,
            out: &mut Vec<Span>,
        ) {
            if at == parts.len() {
                let lo = chosen.iter().map(|s| s.0).min().unwrap_or(0);
                let hi = chosen.iter().map(|s| s.1).max().unwrap_or(0);
                out.push((lo, hi));
                return;
            }
            for span in &parts[at] {
                if no_overlap && chosen.iter().any(|t| span.0 <= t.1 && t.0 <= span.1) {
                    continue;
                }
                chosen.push(*span);
                walk(parts, at + 1, chosen, no_overlap, out);
                chosen.pop();
            }
        }
        // the search is over every way of choosing one span per part, which is
        // small for the rules a query is written by hand with
        let combinations: usize = parts.iter().map(|p| p.len().max(1)).product();
        if combinations <= 100_000 {
            walk(parts, 0, &mut chosen, no_overlap, &mut out);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Is one word within so many edits of another?
fn levenshtein_within(a: &str, b: &str, edits: usize) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > edits {
        return false;
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
    prev[b.len()] <= edits
}

/// The regex source a wildcard pattern stands for.
fn wildcard_to_regex_source(pat: &str) -> String {
    let mut out = String::new();
    for c in pat.chars() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            c if "\\.+()|[]{}^$#&-~".contains(c) => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}
