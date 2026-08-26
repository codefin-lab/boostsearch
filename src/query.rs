//! OpenSearch query DSL -> tantivy queries.

use crate::store::{Fields, Mapping};
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::ops::Bound;
use std::sync::Arc;
use tantivy::query::{
    AllQuery, AutomatonWeight, BooleanQuery, BoostQuery, EmptyQuery, EnableScoring, ExistsQuery,
    FuzzyTermQuery, Occur, PhraseQuery, Query, RangeQuery, TermQuery, Weight,
};
use tantivy::schema::{Field, IndexRecordOption, Term, Type};
use tantivy::{Index, TantivyError};
use tantivy_fst::Regex;

pub struct Ctx<'a> {
    pub fields: &'a Fields,
    pub mapping: &'a Mapping,
    pub index: &'a Index,
    pub max_terms_count: usize,
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
            Some("keyword") | Some("constant_keyword") | Some("wildcard") | Some("ip") => View::Raw,
            Some(_) => View::Dyn, // numeric, date, boolean: identical in both views
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
        if self.mapping.type_of(field).is_none() {
            if let Some(base) = field.strip_suffix(".keyword") {
                if self.mapping.type_of(base).is_some() || base.contains('.') || true {
                    return (self.fields.raw, base.to_string(), View::Raw);
                }
            }
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

fn term_for(field: Field, path: &str, v: &Value) -> Vec<Term> {
    let base = Term::from_field_json_path(field, path, true);
    match v {
        Value::String(s) => {
            let mut t = base.clone();
            t.append_type_and_str(s);
            let mut out = vec![t];
            // strings that were indexed as dates need a date term too
            if let Some(dt) = parse_datetime(s) {
                let mut d = base;
                d.append_type_and_fast_value(dt);
                out.push(d);
            }
            out
        }
        Value::Bool(b) => {
            let mut t = base;
            t.append_type_and_fast_value(*b);
            vec![t]
        }
        Value::Number(n) => {
            let mut out = Vec::new();
            if let Some(i) = n.as_i64() {
                let mut t = base.clone();
                t.append_type_and_fast_value(i);
                out.push(t);
            }
            if let Some(u) = n.as_u64() {
                let mut t = base.clone();
                t.append_type_and_fast_value(u);
                out.push(t);
            }
            if let Some(f) = n.as_f64() {
                let mut t = base.clone();
                t.append_type_and_fast_value(f);
                out.push(t);
            }
            out
        }
        _ => vec![],
    }
}

pub fn parse_datetime(s: &str) -> Option<tantivy::DateTime> {
    use tantivy::time::format_description::well_known::Rfc3339;
    tantivy::time::OffsetDateTime::parse(s, &Rfc3339).ok().map(tantivy::DateTime::from_utc)
}

fn any_of(terms: Vec<Term>) -> Box<dyn Query> {
    match terms.len() {
        0 => Box::new(EmptyQuery),
        1 => Box::new(TermQuery::new(terms.into_iter().next().unwrap(), IndexRecordOption::Basic)),
        _ => Box::new(BooleanQuery::new(
            terms
                .into_iter()
                .map(|t| {
                    (Occur::Should, Box::new(TermQuery::new(t, IndexRecordOption::Basic)) as Box<dyn Query>)
                })
                .collect(),
        )),
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
    fn weight(&self, _s: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(AutomatonWeight::<Regex>::new_for_json_path(
            self.field,
            self.regex.clone(),
            &self.json_path_bytes,
        )))
    }
}

fn json_path_bytes(field: Field, path: &str) -> Vec<u8> {
    // With nothing appended yet, the value bytes are exactly
    // `<json path><JSON_END_OF_PATH>` -- which is what AutomatonWeight wants.
    Term::from_field_json_path(field, path, true).serialized_value_bytes().to_vec()
}

/// The automaton runs over the whole serialised term, not just the text, so a
/// pattern has to be anchored with `<json path>\0<type byte>` first.
fn json_term_prefix_regex(field: Field, path: &str) -> String {
    let mut t = Term::from_field_json_path(field, path, true);
    t.append_type_and_str("");
    t.serialized_value_bytes().iter().map(|b| format!("\\x{b:02x}")).collect()
}

fn regex_query(field: Field, path: &str, pattern: &str) -> Result<Box<dyn Query>> {
    let anchored = format!("{}{pattern}", json_term_prefix_regex(field, path));
    let re = Regex::new(&anchored)
        .map_err(|e| anyhow!("bad regex `{pattern}`: {e}"))?;
    Ok(Box::new(JsonAutomatonQuery {
        field,
        regex: Arc::new(re),
        json_path_bytes: json_path_bytes(field, path),
    }))
}

pub fn wildcard_to_regex(pat: &str) -> String {
    let mut s = String::new();
    for c in pat.chars() {
        match c {
            '*' => s.push_str(".*"),
            '?' => s.push('.'),
            c if "[]{}()|+.\\^$".contains(c) => {
                s.push('\\');
                s.push(c);
            }
            c => s.push(c),
        }
    }
    s
}

/// Map OpenSearch analyzer names onto the tokenizers tantivy ships.
fn tokenizer_name(analyzer: Option<&str>) -> &str {
    match analyzer.unwrap_or("standard") {
        "whitespace" => "whitespace",
        "keyword" | "raw" => "raw",
        "english" | "en_stem" => "en_stem",
        _ => "default",
    }
}

fn analyze(ctx: &Ctx, view: View, text: &str) -> Vec<String> {
    analyze_with(ctx, view, text, None)
}

fn analyze_with(ctx: &Ctx, view: View, text: &str, analyzer: Option<&str>) -> Vec<String> {
    if view == View::Raw && analyzer.is_none() {
        return vec![text.to_string()];
    }
    let name = tokenizer_name(analyzer);
    let mut out = Vec::new();
    if let Ok(mut tk) = ctx
        .index
        .tokenizers()
        .get(name)
        .ok_or(TantivyError::InvalidArgument("no tokenizer".into()))
    {
        let mut stream = tk.token_stream(text);
        while stream.advance() {
            out.push(stream.token().text.clone());
        }
    }
    if out.is_empty() && !text.is_empty() {
        out.push(text.to_lowercase());
    }
    out
}

fn single_key(o: &Value) -> Result<(String, Value)> {
    let obj = o.as_object().ok_or_else(|| anyhow!("expected object"))?;
    let (k, v) = obj.iter().next().ok_or_else(|| anyhow!("empty query clause"))?;
    Ok((k.clone(), v.clone()))
}

/// Extract `{"field": value}` or `{"field": {"value": v, ...}}`.
fn field_and_value(v: &Value) -> Result<(String, Value, Value)> {
    let (field, body) = single_key(v)?;
    if let Some(o) = body.as_object() {
        if let Some(val) = o.get("value").or_else(|| o.get("query")) {
            return Ok((field, val.clone(), body.clone()));
        }
    }
    Ok((field, body.clone(), Value::Null))
}

pub fn build(ctx: &Ctx, q: &Value) -> Result<Box<dyn Query>> {
    if let Some(o) = q.as_object() {
        if o.len() > 1 {
            let extra: Vec<&str> = o.keys().skip(1).map(|s| s.as_str()).collect();
            return Err(anyhow!(
                "[query] malformed query, expected [END_OBJECT] but found [{}]",
                extra.join(", ")
            ));
        }
    }
    let (kind, body) = single_key(q)?;
    let inner: Box<dyn Query> = match kind.as_str() {
        "match_all" => {
            let boost = body.get("boost").and_then(|b| b.as_f64());
            let base: Box<dyn Query> = Box::new(AllQuery);
            match boost {
                Some(b) => Box::new(BoostQuery::new(base, b as f32)),
                None => base,
            }
        }
        "match_none" => Box::new(EmptyQuery),
        "term" => {
            let (field, val, _) = field_and_value(&body)?;
            let (f, path, _) = ctx.resolve(&field, false);
            any_of(term_for(f, &path, &val))
        }
        "terms" => {
            let (field, vals) = single_key(&body)?;
            if let Some(n) = vals.as_array().map(|a| a.len()) {
                if n > ctx.max_terms_count {
                    return Err(anyhow!(
                        "The number of terms [{n}] used in the Terms Query request has exceeded \
                         the allowed maximum of [{}].",
                        ctx.max_terms_count
                    ));
                }
            }
            let (f, path, _) = ctx.resolve(&field, false);
            let arr = vals.as_array().cloned().unwrap_or_default();
            let mut terms = Vec::new();
            for v in &arr {
                terms.extend(term_for(f, &path, v));
            }
            any_of(terms)
        }
        "ids" => {
            let arr = body.get("values").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            let terms: Vec<Term> = arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| Term::from_field_text(ctx.fields.id, s))
                .collect();
            any_of(terms)
        }
        "exists" => {
            let field = body.get("field").and_then(|f| f.as_str()).unwrap_or_default();
            let col = ctx.column_name(field, false);
            Box::new(ExistsQuery::new(col, true))
        }
        "prefix" => {
            let (field, val, _) = field_and_value(&body)?;
            let (f, path, view) = ctx.resolve(&field, true);
            let text = val.as_str().unwrap_or_default();
            let text = if view == View::Dyn { text.to_lowercase() } else { text.to_string() };
            regex_query(f, &path, &format!("{}.*", escape_regex(&text)))?
        }
        "wildcard" => {
            let (field, val, _) = field_and_value(&body)?;
            let (f, path, view) = ctx.resolve(&field, true);
            let text = val.as_str().unwrap_or_default();
            let text = if view == View::Dyn { text.to_lowercase() } else { text.to_string() };
            regex_query(f, &path, &wildcard_to_regex(&text))?
        }
        "regexp" => {
            let (field, val, _) = field_and_value(&body)?;
            let (f, path, _) = ctx.resolve(&field, true);
            regex_query(f, &path, val.as_str().unwrap_or_default())?
        }
        "fuzzy" => {
            let (field, val, opts) = field_and_value(&body)?;
            let (f, path, _) = ctx.resolve(&field, true);
            let d = opts.get("fuzziness").and_then(|v| v.as_u64()).unwrap_or(2).min(2) as u8;
            let mut t = Term::from_field_json_path(f, &path, true);
            t.append_type_and_str(&val.as_str().unwrap_or_default().to_lowercase());
            Box::new(FuzzyTermQuery::new(t, d, true))
        }
        "range" => build_range(ctx, &body)?,
        "match_bool_prefix" => build_match_bool_prefix(ctx, &body)?,
        "query_string" | "simple_query_string" => build_query_string(ctx, &body)?,
        "match" | "match_phrase" | "match_phrase_prefix" => build_match(ctx, &kind, &body)?,
        "multi_match" => build_multi_match(ctx, &body)?,
        // combined_fields scores across fields as one; cross_fields is the
        // closest thing we can assemble from per-field matches
        "combined_fields" => {
            let mut b = body.clone();
            b["type"] = serde_json::json!("cross_fields");
            build_multi_match(ctx, &b)?
        }
        "bool" => build_bool(ctx, &body)?,
        "constant_score" => {
            let f = body.get("filter").ok_or_else(|| anyhow!("constant_score needs filter"))?;
            let boost = body.get("boost").and_then(|b| b.as_f64()).unwrap_or(1.0) as f32;
            Box::new(BoostQuery::new(
                Box::new(tantivy::query::ConstScoreQuery::new(build(ctx, f)?, 1.0)),
                boost,
            ))
        }
        "boosting" => {
            let pos = body.get("positive").ok_or_else(|| anyhow!("boosting needs positive"))?;
            build(ctx, pos)?
        }
        "dis_max" => {
            let qs = body.get("queries").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            let subs: Result<Vec<_>> = qs.iter().map(|s| build(ctx, s)).collect();
            Box::new(tantivy::query::DisjunctionMaxQuery::new(subs?))
        }
        other => return Err(anyhow!("unsupported query type [{other}]")),
    };

    let boost = q
        .get(&kind)
        .and_then(|b| b.get("boost"))
        .and_then(|b| b.as_f64())
        .filter(|_| kind != "match_all" && kind != "constant_score");
    Ok(match boost {
        Some(b) => Box::new(BoostQuery::new(inner, b as f32)),
        None => inner,
    })
}

fn build_bool(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    let mut should_count = 0usize;
    let mut has_must_or_filter = false;

    for (key, occur) in [
        ("must", Occur::Must),
        ("filter", Occur::Must),
        ("should", Occur::Should),
        ("must_not", Occur::MustNot),
    ] {
        let Some(v) = body.get(key) else { continue };
        let list: Vec<Value> = match v {
            Value::Array(a) => a.clone(),
            Value::Null => vec![],
            other => vec![other.clone()],
        };
        for item in list {
            let sub = build(ctx, &item)?;
            let sub: Box<dyn Query> = if key == "filter" {
                Box::new(tantivy::query::ConstScoreQuery::new(sub, 0.0))
            } else {
                sub
            };
            if occur == Occur::Should {
                should_count += 1;
            }
            if key == "must" || key == "filter" {
                has_must_or_filter = true;
            }
            clauses.push((occur, sub));
        }
    }

    if clauses.is_empty() {
        return Ok(Box::new(AllQuery));
    }
    // a bool with only must_not still needs a positive base
    if clauses.iter().all(|(o, _)| *o == Occur::MustNot) {
        clauses.push((Occur::Must, Box::new(AllQuery)));
    }

    let msm = body.get("minimum_should_match").and_then(parse_msm);
    let required = match msm {
        Some(n) => resolve_msm(n, should_count),
        None if !has_must_or_filter && should_count > 0 => 1,
        _ => 0,
    };
    Ok(Box::new(BooleanQuery::with_minimum_required_clauses(clauses, required)))
}

fn parse_msm(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn resolve_msm(n: i64, should_count: usize) -> usize {
    if n < 0 {
        (should_count as i64 + n).max(0) as usize
    } else {
        (n as usize).min(should_count)
    }
}

fn build_match(ctx: &Ctx, kind: &str, body: &Value) -> Result<Box<dyn Query>> {
    let (field, val, opts) = field_and_value(body)?;
    let (f, path, view) = ctx.resolve(&field, true);
    let text = match &val {
        Value::String(s) => s.clone(),
        other => other.to_string().trim_matches('"').to_string(),
    };
    let analyzer = opts.get("analyzer").and_then(|v| v.as_str());
    let tokens = analyze_with(ctx, view, &text, analyzer);
    if tokens.is_empty() {
        return Ok(Box::new(EmptyQuery));
    }
    let terms: Vec<Term> = tokens
        .iter()
        .map(|t| {
            let mut term = Term::from_field_json_path(f, &path, true);
            term.append_type_and_str(t);
            term
        })
        .collect();

    if kind == "match_phrase" || kind == "match_phrase_prefix" {
        if terms.len() == 1 {
            return Ok(Box::new(TermQuery::new(terms[0].clone(), IndexRecordOption::WithFreqs)));
        }
        return Ok(Box::new(PhraseQuery::new(terms)));
    }

    // non-string match on a numeric/keyword field falls back to an exact term
    if view == View::Raw || !matches!(val, Value::String(_)) {
        let mut exact = term_for(f, &path, &val);
        if exact.is_empty() {
            exact = terms.clone();
        }
        return Ok(any_of(exact));
    }

    let operator = opts
        .get("operator")
        .and_then(|o| o.as_str())
        .unwrap_or("or")
        .to_ascii_lowercase();
    let occur = if operator == "and" { Occur::Must } else { Occur::Should };
    let n = terms.len();
    let clauses: Vec<(Occur, Box<dyn Query>)> = terms
        .into_iter()
        .map(|t| {
            (occur, Box::new(TermQuery::new(t, IndexRecordOption::WithFreqs)) as Box<dyn Query>)
        })
        .collect();
    let required = if occur == Occur::Should {
        let msm = opts.get("minimum_should_match").and_then(parse_msm).unwrap_or(1);
        resolve_msm(msm, n)
    } else {
        0
    };
    Ok(Box::new(BooleanQuery::with_minimum_required_clauses(clauses, required)))
}

fn build_multi_match(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
    let kind = body.get("type").and_then(|v| v.as_str()).unwrap_or("best_fields");
    if kind == "bool_prefix" {
        for banned in ["slop", "cutoff_frequency"] {
            if body.get(banned).is_some() {
                return Err(anyhow!("[{banned}] not allowed for type [bool_prefix]"));
            }
        }
    }
    let q = body.get("query").cloned().unwrap_or(Value::Null);
    let fields = body.get("fields").and_then(|f| f.as_array()).cloned().unwrap_or_default();

    // per-field options are the multi_match options minus its own keys
    let mut shared = serde_json::Map::new();
    if let Some(o) = body.as_object() {
        for (k, v) in o {
            if !matches!(k.as_str(), "query" | "fields" | "type" | "boost" | "tie_breaker") {
                shared.insert(k.clone(), v.clone());
            }
        }
    }

    let mut subs: Vec<Box<dyn Query>> = Vec::new();
    for f in fields {
        let Some(spec) = f.as_str() else { continue };
        let (name, boost) = match spec.split_once('^') {
            Some((n, b)) => (n, b.parse::<f32>().ok()),
            None => (spec, None),
        };
        let mut per = shared.clone();
        per.insert("query".into(), q.clone());
        let clause = Value::Object(
            [(name.to_string(), Value::Object(per))].into_iter().collect(),
        );
        let sub = match kind {
            "bool_prefix" => build_match_bool_prefix(ctx, &clause)?,
            "phrase" => build_match(ctx, "match_phrase", &clause)?,
            "phrase_prefix" => build_match(ctx, "match_phrase_prefix", &clause)?,
            _ => build_match(ctx, "match", &clause)?,
        };
        subs.push(match boost {
            Some(b) => Box::new(BoostQuery::new(sub, b)),
            None => sub,
        });
    }
    if subs.is_empty() {
        return Ok(Box::new(EmptyQuery));
    }
    if subs.len() == 1 {
        return Ok(subs.into_iter().next().unwrap());
    }
    // most_fields/cross_fields sum the per-field scores; best_fields takes the best
    if kind == "most_fields" || kind == "cross_fields" || kind == "bool_prefix" {
        Ok(Box::new(BooleanQuery::union(subs)))
    } else {
        Ok(Box::new(tantivy::query::DisjunctionMaxQuery::new(subs)))
    }
}

fn build_range(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
    let (field, spec) = single_key(body)?;
    let (f, path, _) = ctx.resolve(&field, false);
    let get = |keys: [&str; 2]| -> Option<(Value, bool)> {
        for (i, k) in keys.iter().enumerate() {
            if let Some(v) = spec.get(*k) {
                if !v.is_null() {
                    return Some((v.clone(), i == 0));
                }
            }
        }
        None
    };
    let mut lower = get(["gte", "gt"]);
    let mut upper = get(["lte", "lt"]);
    // OpenSearch's default date format accepts a bare year; our date values are
    // indexed as ISO strings, which compare correctly lexicographically
    if ctx.mapping.type_of(&field).map(|t| t == "date").unwrap_or(false) {
        for b in [&mut lower, &mut upper] {
            if let Some((Value::Number(n), _)) = b.as_ref().map(|(v, i)| (v.clone(), *i)) {
                if let Some((_, inclusive)) = b.take() {
                    *b = Some((Value::String(n.to_string()), inclusive));
                }
            }
        }
    }
    if lower.is_none() && upper.is_none() {
        return Ok(Box::new(AllQuery));
    }

    let sample = lower.as_ref().or(upper.as_ref()).map(|(v, _)| v.clone()).unwrap_or(Value::Null);
    let types: Vec<Type> = match &sample {
        Value::Number(n) => {
            if n.is_f64() && n.as_i64().is_none() {
                vec![Type::F64]
            } else if matches!(n.as_i64(), Some(y) if (1000..=9999).contains(&y))
                && !is_numeric_type(ctx.mapping.type_of(&field))
            {
                // a bare year may address a date field whose values are indexed
                // as ISO strings, which compare correctly as text
                vec![Type::I64, Type::U64, Type::F64, Type::Str]
            } else {
                vec![Type::I64, Type::U64, Type::F64]
            }
        }
        Value::String(s) if parse_datetime(s).is_some() => vec![Type::Date, Type::Str],
        Value::String(_) => vec![Type::Str],
        _ => vec![Type::Str],
    };

    let mut subs: Vec<Box<dyn Query>> = Vec::new();
    for t in types {
        let lo = bound_term(f, &path, lower.as_ref(), t, true);
        let hi = bound_term(f, &path, upper.as_ref(), t, false);
        if matches!(lo, Bound::Unbounded) && matches!(hi, Bound::Unbounded) {
            continue;
        }
        subs.push(Box::new(RangeQuery::new(lo, hi)));
    }
    match subs.len() {
        0 => Ok(Box::new(EmptyQuery)),
        1 => Ok(subs.into_iter().next().unwrap()),
        _ => Ok(Box::new(BooleanQuery::new(
            subs.into_iter().map(|s| (Occur::Should, s)).collect(),
        ))),
    }
}

fn is_numeric_type(t: Option<&str>) -> bool {
    matches!(
        t,
        Some("long") | Some("integer") | Some("short") | Some("byte") | Some("double")
            | Some("float") | Some("half_float") | Some("scaled_float") | Some("unsigned_long")
    )
}

fn bound_term(
    f: Field,
    path: &str,
    b: Option<&(Value, bool)>,
    ty: Type,
    _is_lower: bool,
) -> Bound<Term> {
    let Some((v, inclusive)) = b else { return Bound::Unbounded };
    let base = Term::from_field_json_path(f, path, true);
    let mut t = base;
    match (ty, v) {
        (Type::I64, Value::Number(n)) => t.append_type_and_fast_value(n.as_i64().unwrap_or(0)),
        (Type::U64, Value::Number(n)) => match n.as_u64() {
            Some(u) => t.append_type_and_fast_value(u),
            None => return Bound::Unbounded,
        },
        (Type::F64, Value::Number(n)) => t.append_type_and_fast_value(n.as_f64().unwrap_or(0.0)),
        (Type::Date, Value::String(s)) => match parse_datetime(s) {
            Some(d) => t.append_type_and_fast_value(d),
            None => return Bound::Unbounded,
        },
        (Type::Str, Value::String(s)) => t.append_type_and_str(s),
        (Type::Str, other) => t.append_type_and_str(&other.to_string()),
        _ => return Bound::Unbounded,
    }
    if *inclusive { Bound::Included(t) } else { Bound::Excluded(t) }
}


/// `match_bool_prefix`: every analysed term is a term query except the last,
/// which matches as a prefix.
fn build_match_bool_prefix(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
    let (field, val, opts) = field_and_value(body)?;
    for banned in ["slop", "cutoff_frequency"] {
        if opts.get(banned).is_some() {
            return Err(anyhow!("[{banned}] not allowed for type [bool_prefix]"));
        }
    }
    let (f, path, view) = ctx.resolve(&field, true);
    let text = val.as_str().unwrap_or_default();
    let analyzer = opts.get("analyzer").and_then(|v| v.as_str());
    let tokens = analyze_with(ctx, view, text, analyzer);
    if tokens.is_empty() {
        return Ok(Box::new(EmptyQuery));
    }
    let operator = opts
        .get("operator")
        .and_then(|o| o.as_str())
        .unwrap_or("or")
        .to_ascii_lowercase();
    let occur = if operator == "and" { Occur::Must } else { Occur::Should };
    let fuzziness = opts
        .get("fuzziness")
        .and_then(|v| match v {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.trim_start_matches("AUTO").parse::<u64>().ok().or(Some(1)),
            _ => None,
        })
        .map(|d| d.min(2) as u8);
    let last = tokens.len() - 1;
    let n = tokens.len();
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for (i, tok) in tokens.iter().enumerate() {
        // fuzziness applies to the term clauses only; the final term is always
        // a plain prefix query, matching OpenSearch's documented behaviour
        let sub: Box<dyn Query> = if i == last {
            // the prefix automaton scores as a constant; OR-ing the exact term
            // back in restores BM25 weighting for documents that really contain it
            let prefix = regex_query(f, &path, &format!("{}.*", escape_regex(tok)))?;
            let mut exact = Term::from_field_json_path(f, &path, true);
            exact.append_type_and_str(tok);
            Box::new(BooleanQuery::union(vec![
                Box::new(TermQuery::new(exact, IndexRecordOption::WithFreqs)) as Box<dyn Query>,
                prefix,
            ]))
        } else if let Some(d) = fuzziness {
            let mut t = Term::from_field_json_path(f, &path, true);
            t.append_type_and_str(tok);
            Box::new(FuzzyTermQuery::new(t, d, true))
        } else {
            let mut t = Term::from_field_json_path(f, &path, true);
            t.append_type_and_str(tok);
            Box::new(TermQuery::new(t, IndexRecordOption::WithFreqs))
        };
        clauses.push((occur, sub));
    }
    let required = if occur == Occur::Should {
        let msm = opts.get("minimum_should_match").and_then(parse_msm).unwrap_or(1);
        resolve_msm(msm, n)
    } else {
        0
    };
    Ok(Box::new(BooleanQuery::with_minimum_required_clauses(clauses, required)))
}

fn escape_regex(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "[]{}()|+*?.\\^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A pragmatic `query_string` subset: `field:term`, quoted phrases, wildcards,
/// AND/OR/NOT, and `default_field` / `default_operator`.
fn build_query_string(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
    let text = body.get("query").and_then(|v| v.as_str()).unwrap_or_default();
    let default_operator = body
        .get("default_operator")
        .and_then(|v| v.as_str())
        .unwrap_or("or")
        .to_ascii_lowercase();
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
            Some((f, v)) if !f.is_empty() && !f.contains(' ') => (Some(f.to_string()), v.to_string()),
            _ => (None, tok.clone()),
        };
        let targets: Vec<String> =
            field_part.map(|f| vec![f]).unwrap_or_else(|| default_fields.clone());
        let value = value.trim_matches('"').to_string();
        if value.is_empty() {
            continue;
        }
        let regex_literal = value.len() > 2 && value.starts_with('/') && value.ends_with('/');
        let mut per_field: Vec<Box<dyn Query>> = Vec::new();
        for name in &targets {
            let (f, path, view) = ctx.resolve(name, true);
            if regex_literal {
                if let Ok(q) = regex_query(f, &path, &value[1..value.len() - 1]) {
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
        let sub: Box<dyn Query> = if per_field.len() == 1 {
            per_field.into_iter().next().unwrap()
        } else {
            Box::new(BooleanQuery::union(per_field))
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
    let required = if should_count > 0 && clauses.iter().all(|(o, _)| *o != Occur::Must) { 1 } else { 0 };
    Ok(Box::new(BooleanQuery::with_minimum_required_clauses(clauses, required)))
}

/// Split on whitespace, keeping quoted phrases (and `field:"phrase"`) together.
fn split_query_string(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            c if c.is_whitespace() && !in_quotes => {
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
