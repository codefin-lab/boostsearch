//! OpenSearch query DSL -> BoostCore queries.

use crate::store::{Fields, Mapping};
use anyhow::{Result, anyhow};
use boostcore::query::{
    AllQuery, AutomatonWeight, BooleanQuery, BoostQuery, EmptyQuery, EnableScoring, ExistsQuery,
    FuzzyTermQuery, Occur, PhraseQuery, Query, RangeQuery, TermQuery, Weight,
};
use boostcore::schema::{Field, IndexRecordOption, TantivyDocument, Term, Type, Value as _};
use boostcore::{Index, TantivyError};
use boostcore_fst::Regex;
use serde_json::Value;
use std::ops::Bound;
use std::sync::Arc;

mod dispatch;
pub(crate) use dispatch::*;

mod script;
pub(crate) use script::*;
mod shards;
pub(crate) use shards::OnShards;

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
mod combined;
mod intervals;
pub(crate) mod positions;
mod sloppy;
pub(crate) mod spans;
pub(crate) use combined::CombinedTerm;
pub(crate) use intervals::build_intervals;
pub(crate) use sloppy::SloppyPhrase;
pub(crate) use spans::*;
mod graph;
pub(crate) use graph::*;
mod fuzzy;
mod text;
pub(crate) use fuzzy::ScoredFuzzy;
pub(crate) use text::*;

pub struct Ctx<'a> {
    pub fields: &'a Fields,
    pub mapping: &'a Mapping,
    pub index: &'a Index,
    /// the analyzers this index knows, by the names the mapping uses
    pub analysis: &'a crate::analysis::Registry,
    pub max_terms_count: usize,
    pub max_regex_length: usize,
    /// whether the cluster still allows the queries that cost the most to run
    pub allow_expensive: bool,
    /// value kinds seen per field path, used to narrow typed range variants
    pub observed_kinds: &'a std::collections::HashMap<String, u8>,
    pub kinds_complete: bool,
    pub stats: &'a std::sync::Arc<crate::blockstats::StatsCache>,
    /// the vectors this index holds, which a `knn` query reads and nothing
    /// else does
    pub vectors: &'a parking_lot::RwLock<crate::knn::Vectors>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum View {
    Dyn,
    Raw,
    /// the analysed words with a column over them, which only a text field
    /// that declared `fielddata: true` has
    Fielddata,
}

impl<'a> Ctx<'a> {
    pub fn field_of(&self, v: View) -> Field {
        match v {
            View::Dyn => self.fields.dynamic,
            View::Raw => self.fields.raw,
            View::Fielddata => self.fields.fielddata,
        }
    }

    /// Which of the two JSON views backs this field name.
    ///
    /// `analyzed` is true for full-text contexts (`match`), false for exact ones
    /// (`term`, sorting, term aggregations) -- mirroring the text/keyword split.
    pub fn view(&self, field: &str, analyzed: bool) -> View {
        // a path inside a flat_object is exact, like a keyword; the mapping
        // never names the path itself, so the ancestor has to be consulted
        if self.mapping.type_of(field).is_none() {
            let mut prefix = field;
            while let Some((head, _)) = prefix.rsplit_once('.') {
                if self.mapping.type_of(head) == Some("flat_object") {
                    return View::Raw;
                }
                prefix = head;
            }
        }
        match self.mapping.type_of(field) {
            // A declared text field is its analysed words, and that is what a
            // name standing alone asks about -- a `term` against it matches a
            // word, not the whole value. Its untouched view, where it has one,
            // is addressed as `field.keyword` and resolved before this.
            // Sorting or aggregating asks for a column, which such a field has
            // only where the mapping asked for one.
            Some("text")
            | Some("match_only_text")
            | Some("search_as_you_type")
            | Some("annotated_text") => {
                if !analyzed && self.mapping.views_of(field).fielddata {
                    View::Fielddata
                } else {
                    View::Dyn
                }
            }
            // everything else declared is exact, and lives untouched
            Some(_) => View::Raw,
            // Nothing declared. The value was written both ways, the way
            // OpenSearch's dynamic mapping writes a string as a text field
            // with a keyword sub-field, so the context decides: words for a
            // `match`, the value as it arrived for everything else.
            None => {
                if analyzed {
                    View::Dyn
                } else {
                    View::Raw
                }
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
        // a plain keyword sub-field -- no normalizer -- holds what the raw view
        // of its parent already holds, so it is read from there rather than
        // from a copy of its own
        if let Some((parent, _)) = field.rsplit_once('.')
            && self.mapping.plain_keyword_sub(field)
        {
            return (self.fields.raw, parent.to_string(), View::Raw);
        }
        let v = self.view(field, analyzed);
        (self.field_of(v), field.to_string(), v)
    }

    /// Whether a document has any value under this field.
    ///
    /// The untouched view carries a column, and a column knows directly. A
    /// field that is only ever analysed has no column to ask -- it has
    /// postings, so the question becomes whether any term stands under its
    /// path, which is the same question OpenSearch answers out of
    /// `_field_names`.
    pub fn exists_query(&self, field: &str) -> Result<Box<dyn Query>> {
        let (f, path, view) = self.resolve(field, false);
        if matches!(view, View::Raw | View::Fielddata) {
            return Ok(Box::new(ExistsQuery::new(self.column_name(field, false), true)));
        }
        crate::query::pattern::regex_query(f, &path, ".*")
    }

    /// Where a comparison reads a field from.
    ///
    /// A range compares values, not words: `gte: "192.168.0.3"` is a question
    /// about the string that arrived, and a field the dynamic mapping happened
    /// to call text still has the value it arrived as in the untouched view.
    pub fn resolve_exact(&self, field: &str) -> (Field, String, View) {
        let (f, path, view) = self.resolve(field, false);
        if view == View::Dyn && self.mapping.views_of(field).untouched {
            return (self.fields.raw, path, View::Raw);
        }
        (f, path, view)
    }

    pub fn column_name(&self, field: &str, analyzed: bool) -> String {
        let (_, path, view) = self.resolve(field, analyzed);
        let prefix = match view {
            View::Raw => crate::store::RAW,
            View::Fielddata => crate::store::FIELDDATA,
            View::Dyn => crate::store::DYN,
        };
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
    // whether the last clause is required only because the default operator
    // is `and`: an `OR` after it makes it optional again, where one required
    // by `+` or `AND` stays required
    let mut last_by_default = false;

    for tok in split_query_string(text) {
        match tok.to_ascii_uppercase().as_str() {
            // `a AND b` requires both of them: the word before the operator
            // is required as much as the word after it
            "AND" | "&&" | "+" => {
                if let Some(last) = clauses.last_mut()
                    && last.0 == Occur::Should
                {
                    last.0 = Occur::Must;
                }
                pending_occur = Some(Occur::Must);
                continue;
            }
            "OR" | "||" | "|" => {
                // `quick | dog` is either word, whatever the default: with
                // `default_operator: and` the word before the bar had been
                // made required, and a document with only `dog` was lost
                if last_by_default
                    && let Some(last) = clauses.last_mut()
                    && last.0 == Occur::Must
                {
                    last.0 = Occur::Should;
                    should_count += 1;
                }
                pending_occur = Some(Occur::Should);
                continue;
            }
            "NOT" | "!" => {
                pending_not = true;
                continue;
            }
            _ => {}
        }
        // `+word` requires it and `-word` refuses it, which is how a simple
        // query string writes AND and NOT
        let mut tok = tok;
        if let Some(rest) = tok.strip_prefix('+') {
            if let Some(last) = clauses.last_mut()
                && last.0 == Occur::Should
            {
                last.0 = Occur::Must;
            }
            pending_occur = Some(Occur::Must);
            tok = rest.to_string();
        } else if let Some(rest) = tok.strip_prefix('-') {
            pending_not = true;
            tok = rest.to_string();
        }
        if tok.is_empty() {
            continue;
        }
        let (field_part, value) = match tok.split_once(':') {
            Some((f, v)) if !f.is_empty() && !f.contains(' ') => {
                (Some(f.to_string()), v.to_string())
            }
            _ => (None, tok.clone()),
        };
        let targets: Vec<String> =
            field_part.map(|f| vec![f]).unwrap_or_else(|| default_fields.clone());
        // A value in quotes is a phrase, and `~N` after it is how far apart
        // its words may stand. The quotes were only stripped, so `"brown
        // fox"` looked for either word anywhere and `+ -lazy` beside it found
        // documents the reference does not.
        let (value, phrase_slop) = match quoted_phrase(&value) {
            Some((inner, slop)) => (inner, Some(slop)),
            None if value.starts_with('[') || value.starts_with('{') => (value, None),
            None => (value.trim_matches('"').to_string(), None),
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
        // `field:(a OR b)` is a group: its words are asked of that field and
        // joined by the operators inside it, as one clause. The group was cut
        // at its spaces, so `lazy)` was looked for in every field, and a
        // document matched on the word in its body as well as its title.
        if value.len() > 2 && value.starts_with('(') && value.ends_with(')') {
            let inner = &value[1..value.len() - 1];
            let mut spec = serde_json::json!({
                "query": inner,
                "fields": targets,
                "default_operator": default_operator,
            });
            if let Some(named) = body.get("analyzer") {
                spec["analyzer"] = named.clone();
            }
            if let Ok(sub) = build_query_string(ctx, &spec) {
                let explicit = pending_occur.is_some();
                let occur = if pending_not {
                    Occur::MustNot
                } else {
                    pending_occur.take().unwrap_or(if default_operator == "and" {
                        Occur::Must
                    } else {
                        Occur::Should
                    })
                };
                last_by_default = !pending_not && !explicit && occur == Occur::Must;
                pending_not = false;
                if occur == Occur::Should {
                    should_count += 1;
                }
                clauses.push((occur, sub));
            }
            continue;
        }
        // `field:[a TO b]` is a range, not a term, and so is `field:>5`
        if let Some(spec) = parse_range_token(&value).or_else(|| comparison_range(&value)) {
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
                let explicit = pending_occur.is_some();
                let occur = if pending_not {
                    Occur::MustNot
                } else {
                    pending_occur.take().unwrap_or(if default_operator == "and" {
                        Occur::Must
                    } else {
                        Occur::Should
                    })
                };
                last_by_default = !pending_not && !explicit && occur == Occur::Must;
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
            if phrase_slop.is_none() && (value.contains('*') || value.contains('?')) {
                let pat = if view == View::Dyn { value.to_lowercase() } else { value.clone() };
                if let Ok(q) = regex_query(f, &path, &wildcard_to_regex(&pat)) {
                    per_field.push(q);
                }
                continue;
            }
            // the query may be cut with an analyzer of its own rather than
            // the one the field was written with
            let mut inner = serde_json::json!({"query": value.clone()});
            if let Some(named) = body.get("analyzer").and_then(|v| v.as_str()) {
                inner["analyzer"] = serde_json::json!(named);
            }
            let kind = match phrase_slop {
                Some(slop) => {
                    inner["slop"] = serde_json::json!(slop);
                    "match_phrase"
                }
                None => "match",
            };
            let clause = serde_json::json!({ name.clone(): inner });
            if let Ok(q) = build_match(ctx, kind, &clause) {
                // a word looked for in a field that is not analysed is either
                // there or not, and scores one either way
                per_field.push(match view {
                    View::Raw => Box::new(ConstScore::new(q, 1.0)),
                    _ => q,
                });
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
        let explicit = pending_occur.is_some();
        let occur = if pending_not {
            Occur::MustNot
        } else {
            pending_occur.take().unwrap_or(if default_operator == "and" {
                Occur::Must
            } else {
                Occur::Should
            })
        };
        last_by_default = !pending_not && !explicit && occur == Occur::Must;
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
            // a group in parentheses is one clause, like a range in brackets
            '[' | '{' | '(' if !in_quotes => {
                depth += 1;
                cur.push(c);
            }
            ']' | '}' | ')' if !in_quotes => {
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

/// `>5`, `>=5`, `<5` and `<=5`, which the query string writes for an open
/// range. They were cut into words like any other value, so `price:>5`
/// looked for the word `5` and found nothing.
fn comparison_range(value: &str) -> Option<serde_json::Value> {
    let (op, rest) = [(">=", "gte"), ("<=", "lte"), (">", "gt"), ("<", "lt")]
        .iter()
        .find_map(|(sym, op)| value.strip_prefix(sym).map(|rest| (*op, rest)))?;
    let rest = rest.trim().trim_matches('"');
    if rest.is_empty() {
        return None;
    }
    Some(serde_json::json!({ op: rest }))
}

/// `"a phrase"` or `"a phrase"~2`: the words, and how far apart they may be.
fn quoted_phrase(value: &str) -> Option<(String, u64)> {
    let rest = value.strip_prefix('"')?;
    let end = rest.rfind('"')?;
    let (inner, after) = (&rest[..end], &rest[end + 1..]);
    let slop = match after {
        "" => 0,
        tail => tail.strip_prefix('~')?.parse().ok()?,
    };
    Some((inner.to_string(), slop))
}
