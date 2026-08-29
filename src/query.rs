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
            Some("keyword") | Some("constant_keyword") | Some("wildcard") | Some("ip")
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
        // naming a flat_object itself asks about every value beneath it
        if self.mapping.type_of(field) == Some("flat_object") {
            let path = format!("{field}.{}", crate::store::FLAT_VALUES);
            let v = self.view(field, analyzed);
            return (self.field_of(v), path, v);
        }
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

/// Put a query term through the same normalizer the field was indexed with,
/// so `ABCD` finds what `lowercase` stored as `abcd`.
fn normalized(ctx: &Ctx, field: &str, text: &str) -> String {
    match ctx.mapping.normalizer_of(field) {
        Some(n) => crate::store::normalize(&Value::String(text.to_string()), &n)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| text.to_string()),
        None => text.to_string(),
    }
}

/// Rewrite a value written as an IP into the form the field was indexed in.
fn ip_value(ctx: &Ctx, field: &str, v: &Value) -> Value {
    match ctx.mapping.type_of(field) {
        Some("ip") => match v.as_str().and_then(crate::store::canonical_ip) {
            Some(c) => Value::String(c),
            None => v.clone(),
        },
        Some("date") | Some("date_nanos") => match crate::store::canonical_date(v) {
            Some(c) => Value::String(c),
            None => v.clone(),
        },
        _ => v.clone(),
    }
}

/// `term` on an `ip` field accepts a CIDR block, which names a range of
/// addresses rather than one of them.
fn ip_term_query(
    ctx: &Ctx,
    field: &str,
    f: Field,
    path: &str,
    v: &Value,
) -> Option<Box<dyn Query>> {
    if ctx.mapping.type_of(field) != Some("ip") {
        return None;
    }
    let s = v.as_str()?;
    if let Some((lo, hi)) = crate::store::canonical_cidr(s) {
        let mut l = Term::from_field_json_path(f, path, true);
        l.append_type_and_str(&lo);
        let mut h = Term::from_field_json_path(f, path, true);
        h.append_type_and_str(&hi);
        return Some(Box::new(RangeQuery::new(
            Bound::Included(l),
            Bound::Included(h),
        )));
    }
    Some(any_of(term_for(f, path, &ip_value(ctx, field, v))))
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
            // 401.0 and 401 name the same value; whichever form was indexed,
            // either spelling of the query has to find it
            let whole = n.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 9.007e15);
            let as_i64 = n.as_i64().or_else(|| whole.map(|f| f as i64));
            let as_u64 = n.as_u64().or_else(|| whole.filter(|f| *f >= 0.0).map(|f| f as u64));
            if let Some(i) = as_i64 {
                let mut t = base.clone();
                t.append_type_and_fast_value(i);
                out.push(t);
            }
            if let Some(u) = as_u64 {
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
    let mut chars = pat.chars();
    while let Some(c) = chars.next() {
        // a backslash makes the next character a literal, `*` and `?` included
        if c == '\\' {
            if let Some(next) = chars.next() {
                if next.is_alphanumeric() {
                    s.push(next);
                } else {
                    s.push('\\');
                    s.push(next);
                }
            }
            continue;
        }
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

/// Tokenise text with a named analyzer, for the `_analyze` endpoint.
pub fn analyze_text(index: &Index, text: &str, analyzer: Option<&str>) -> Vec<String> {
    let name = tokenizer_name(analyzer);
    let mut out = Vec::new();
    if let Some(mut tk) = index.tokenizers().get(name) {
        let mut stream = tk.token_stream(text);
        while stream.advance() {
            out.push(stream.token().text.clone());
        }
    }
    out
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
            let (field, val, opts) = field_and_value(&body)?;
            // `_id` is a field of its own, not part of either JSON view, so a
            // term naming it has to be built against that field directly
            if field == "_id" {
                let text = match &val {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                return Ok(Box::new(ConstScore::new(
                    any_of(vec![Term::from_field_text(ctx.fields.id, &text)]),
                    1.0,
                )));
            }
            let (f, path, view) = ctx.resolve(&field, false);
            if is_true(opts.get("case_insensitive")) {
                if let Some(s) = val.as_str() {
                    return regex_query(f, &path, &case_insensitive_regex(&escape_regex(s)));
                }
            }
            let val = ip_value(ctx, &field, &val);
            if let Some(s) = val.as_str() {
                let n = normalized(ctx, &field, s);
                if n != s {
                    let hit = any_of(term_for(f, &path, &Value::String(n)));
                    return Ok(if view == View::Raw {
                        Box::new(ConstScore::new(hit, 1.0))
                    } else {
                        hit
                    });
                }
            }
            if let Some(q) = ip_term_query(ctx, &field, f, &path, &val) {
                return Ok(q);
            }
            let exact = any_of(term_for(f, &path, &val));
            // an exact match on a field that is not analysed has nothing to
            // rank by: every match is equally exact, so each scores one
            if view == View::Raw {
                Box::new(ConstScore::new(exact, 1.0))
            } else {
                exact
            }
        }
        "terms" => {
            let (field, vals) = single_key(&body)?;
            if field == "_id" {
                let items: Vec<Value> = match &vals {
                    Value::Array(a) => a.clone(),
                    other => vec![other.clone()],
                };
                let terms: Vec<Term> = items
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .map(|s| Term::from_field_text(ctx.fields.id, &s))
                    .collect();
                return Ok(Box::new(ConstScore::new(any_of(terms), 1.0)));
            }
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
            let mut subs: Vec<Box<dyn Query>> = Vec::new();
            for v in &arr {
                // a CIDR entry names a range, not a term, so it cannot join the
                // flat term set the common case builds
                match v.as_str().filter(|s| s.contains('/')).and(
                    ip_term_query(ctx, &field, f, &path, v),
                ) {
                    Some(q) => subs.push(q),
                    None => terms.extend(term_for(f, &path, &ip_value(ctx, &field, v))),
                }
            }
            if subs.is_empty() {
                any_of(terms)
            } else {
                if !terms.is_empty() {
                    subs.push(any_of(terms));
                }
                Box::new(BooleanQuery::new(
                    subs.into_iter().map(|q| (Occur::Should, q)).collect(),
                ))
            }
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
            let (field, val, opts) = field_and_value(&body)?;
            let (f, path, view) = ctx.resolve(&field, true);
            let text = val.as_str().unwrap_or_default();
            let text = if view == View::Dyn { text.to_lowercase() } else { text.to_string() };
            let text = normalized(ctx, &field, &text);
            let pat = escape_regex(&text);
            let pat = if is_true(opts.get("case_insensitive")) {
                case_insensitive_regex(&pat)
            } else {
                pat
            };
            regex_query(f, &path, &format!("{pat}.*"))?
        }
        "wildcard" => {
            let (field, val, opts) = field_and_value(&body)?;
            let (f, path, view) = ctx.resolve(&field, true);
            let text = val.as_str().unwrap_or_default();
            let text = if view == View::Dyn { text.to_lowercase() } else { text.to_string() };
            let text = normalized(ctx, &field, &text);
            let pat = wildcard_to_regex(&text);
            let pat = if is_true(opts.get("case_insensitive")) {
                case_insensitive_regex(&pat)
            } else {
                pat
            };
            regex_query(f, &path, &pat)?
        }
        "regexp" => {
            let (field, val, opts) = field_and_value(&body)?;
            let (f, path, _) = ctx.resolve(&field, true);
            let text = normalized(ctx, &field, val.as_str().unwrap_or_default());
            let pat = if is_true(opts.get("case_insensitive")) {
                case_insensitive_regex(&text)
            } else {
                text
            };
            regex_query(f, &path, &pat)?
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
        "span_near" => build_span_near(ctx, &body)?,
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
                Box::new(ConstScore::new(build(ctx, f)?, 1.0)),
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
                Box::new(ConstScore::new(sub, 0.0))
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
    // naming no field searches them all, which for us is every path a document
    // has actually put a value at
    let fields = match body.get("fields").and_then(|f| f.as_array()) {
        Some(f) if !f.is_empty() => f.clone(),
        _ => ctx
            .observed_kinds
            .keys()
            .filter(|k| !k.starts_with('_'))
            .map(|k| Value::String(k.clone()))
            .collect(),
    };

    // per-field options are the multi_match options minus its own keys
    let mut shared = serde_json::Map::new();
    if let Some(o) = body.as_object() {
        for (k, v) in o {
            if !matches!(k.as_str(), "query" | "fields" | "type" | "boost" | "tie_breaker") {
                shared.insert(k.clone(), v.clone());
            }
        }
    }

    // a field may be named by pattern, which stands for every field it matches
    let expanded: Vec<String> = fields
        .iter()
        .filter_map(|f| f.as_str())
        .flat_map(|spec| {
            let (name, boost) = match spec.split_once('^') {
                Some((n, b)) => (n, Some(b)),
                None => (spec, None),
            };
            if !name.contains('*') {
                return vec![spec.to_string()];
            }
            let mut hits: Vec<String> = ctx
                .mapping
                .types
                .keys()
                .chain(ctx.observed_kinds.keys())
                .filter(|k| crate::store::glob_match(name, k))
                .map(|k| match boost {
                    Some(b) => format!("{k}^{b}"),
                    None => k.clone(),
                })
                .collect();
            hits.sort();
            hits.dedup();
            hits
        })
        .collect();
    let fields: Vec<Value> = if expanded.is_empty() {
        fields
    } else {
        expanded.into_iter().map(Value::String).collect()
    };

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

/// A `*_range` field stores an interval per document, so a range query over it
/// compares two intervals rather than a value against bounds. The stored
/// endpoints are already separate numeric paths, so each relation is a pair of
/// ordinary range queries.
fn build_range_field_query(
    ctx: &Ctx,
    field: &str,
    spec: &Value,
) -> Option<Result<Box<dyn Query>>> {
    let kind = ctx.mapping.type_of(field)?;
    if !kind.ends_with("_range") {
        return None;
    }
    let relation = spec
        .get("relation")
        .and_then(|v| v.as_str())
        .unwrap_or("intersects")
        .to_ascii_lowercase();
    // a date bound may be written as date math, which names a whole unit; the
    // bound decides which end of it is meant
    let bound = |inclusive_key: &str, exclusive_key: &str, up_when_inclusive: bool| {
        let (v, inclusive) = match spec.get(inclusive_key) {
            Some(v) => (v.clone(), true),
            None => (spec.get(exclusive_key)?.clone(), false),
        };
        if !kind.starts_with("date") {
            return Some((v, inclusive));
        }
        let up = if inclusive { up_when_inclusive } else { !up_when_inclusive };
        let rewritten =
            crate::store::canonical_date_bound(&v, up).map(Value::String).unwrap_or(v);
        Some((rewritten, inclusive))
    };
    let q_lo = bound("gte", "gt", false);
    let q_hi = bound("lte", "lt", true);
    // an exclusive query bound stays exclusive in the comparison it becomes:
    // a bucket ending `lt` March does not reach an interval starting on the 1st
    let lower_key = |inclusive: bool| if inclusive { "gte" } else { "gt" };
    let upper_key = |inclusive: bool| if inclusive { "lte" } else { "lt" };
    let lo_field = format!("{field}.gte");
    let hi_field = format!("{field}.lte");

    let mut clauses: Vec<Value> = Vec::new();
    match relation.as_str() {
        // the stored interval overlaps the query interval
        "intersects" => {
            if let Some((hi, inc)) = &q_hi {
                clauses.push(serde_json::json!({"range": {lo_field.clone(): {upper_key(*inc): hi}}}));
            }
            if let Some((lo, inc)) = &q_lo {
                clauses.push(serde_json::json!({"range": {hi_field.clone(): {lower_key(*inc): lo}}}));
            }
        }
        // the stored interval covers the query interval
        "contains" => {
            if let Some((lo, _)) = &q_lo {
                clauses.push(serde_json::json!({"range": {lo_field.clone(): {"lte": lo}}}));
            }
            if let Some((hi, _)) = &q_hi {
                clauses.push(serde_json::json!({"range": {hi_field.clone(): {"gte": hi}}}));
            }
        }
        // the stored interval sits inside the query interval
        "within" => {
            if let Some((lo, inc)) = &q_lo {
                clauses.push(serde_json::json!({"range": {lo_field.clone(): {lower_key(*inc): lo}}}));
            }
            if let Some((hi, inc)) = &q_hi {
                clauses.push(serde_json::json!({"range": {hi_field.clone(): {upper_key(*inc): hi}}}));
            }
        }
        other => {
            return Some(Err(anyhow!("unsupported range relation [{other}]")));
        }
    }
    if clauses.is_empty() {
        // no bounds: every document that has the field at all
        clauses.push(serde_json::json!({"exists": {"field": lo_field}}));
    }
    let combined = serde_json::json!({"bool": {"filter": clauses}});
    Some(build(ctx, &combined))
}

fn build_range(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
    let (field, spec) = single_key(body)?;
    if let Some(r) = build_range_field_query(ctx, &field, &spec) {
        return r;
    }
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
    // `from`/`to` are the older spelling, with inclusivity as its own flag
    let older = |key: &str, flag: &str| -> Option<(Value, bool)> {
        let v = spec.get(key).filter(|v| !v.is_null())?.clone();
        let inclusive = match spec.get(flag) {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => s != "false",
            _ => true,
        };
        Some((v, inclusive))
    };
    let mut lower = get(["gte", "gt"]).or_else(|| older("from", "include_lower"));
    let mut upper = get(["lte", "lt"]).or_else(|| older("to", "include_upper"));
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
    if matches!(ctx.mapping.type_of(&field), Some("ip" | "date" | "date_nanos")) {
        for (is_lower, b) in [(true, &mut lower), (false, &mut upper)] {
            if let Some((v, inclusive)) = b.clone() {
                let up = (is_lower && !inclusive) || (!is_lower && inclusive);
                let rewritten = if matches!(ctx.mapping.type_of(&field), Some("ip")) {
                    ip_value(ctx, &field, &v)
                } else {
                    crate::store::canonical_date_bound(&v, up)
                        .map(Value::String)
                        .unwrap_or(v.clone())
                };
                *b = Some((rewritten, inclusive));
            }
        }
    }
    // A numeric bound against a string field is a lexicographic comparison in
    // OpenSearch -- "5" and "400" are both below 500, "ingesting..." is not.
    if matches!(
        ctx.mapping.type_of(&field),
        Some("keyword" | "text" | "wildcard" | "constant_keyword" | "search_as_you_type"
            | "match_only_text")
    ) {
        for b in [&mut lower, &mut upper] {
            if let Some((Value::Number(n), inclusive)) = b.clone() {
                *b = Some((Value::String(n.to_string()), inclusive));
            }
        }
    }
    if lower.is_none() && upper.is_none() {
        return Ok(Box::new(AllQuery));
    }

    let sample = lower.as_ref().or(upper.as_ref()).map(|(v, _)| v.clone()).unwrap_or(Value::Null);
    // there are only two booleans, so a range over them is just the set of
    // values it admits -- and range scans do not accept the type at all
    if sample.is_boolean() {
        let ok = |b: bool| {
            lower.as_ref().is_none_or(|(v, inc)| match v.as_bool() {
                Some(l) => b > l || (*inc && b == l),
                None => true,
            }) && upper.as_ref().is_none_or(|(v, inc)| match v.as_bool() {
                Some(u) => b < u || (*inc && b == u),
                None => true,
            })
        };
        let terms: Vec<Term> = [false, true]
            .into_iter()
            .filter(|b| ok(*b))
            .flat_map(|b| term_for(f, &path, &Value::Bool(b)))
            .collect();
        return Ok(any_of(terms));
    }
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
        Value::Bool(_) => vec![Type::Bool],
        _ => vec![Type::Str],
    };

    // only build the typed variants this field has actually held; each extra
    // variant is a separate range scan unioned over the whole segment
    // OBSEARCH_NO_KIND_NARROW=1 disables the narrowing, for A/B runs
    let narrowing_on = ctx.kinds_complete && std::env::var("OBSEARCH_NO_KIND_NARROW").is_err();
    let types: Vec<Type> = match ctx.observed_kinds.get(&field).filter(|_| narrowing_on) {
        Some(&kinds) if kinds != 0 => {
            let narrowed: Vec<Type> = types
                .iter()
                .copied()
                .filter(|t| match t {
                    Type::I64 => kinds & crate::store::KIND_I64 != 0,
                    Type::U64 => kinds & crate::store::KIND_U64 != 0,
                    Type::F64 => kinds & crate::store::KIND_F64 != 0,
                    Type::Str => kinds & crate::store::KIND_STR != 0,
                    Type::Date => kinds & crate::store::KIND_DATE != 0,
                    _ => true,
                })
                .collect();
            if narrowed.is_empty() { types } else { narrowed }
        }
        _ => types,
    };

    // A single numeric type means block statistics can drive the scan, which
    // lets whole runs of documents be skipped instead of compared one by one.
    let mut subs: Vec<Box<dyn Query>> = Vec::new();
    for t in types.iter().copied() {
        let lo = bound_term(f, &path, lower.as_ref(), t, true);
        let hi = bound_term(f, &path, upper.as_ref(), t, false);
        if matches!(lo, Bound::Unbounded) && matches!(hi, Bound::Unbounded) {
            continue;
        }
        subs.push(Box::new(RangeQuery::new(lo, hi)));
    }
    let general: Box<dyn Query> = match subs.len() {
        0 => Box::new(EmptyQuery),
        1 => subs.into_iter().next().unwrap(),
        _ => Box::new(BooleanQuery::new(
            subs.into_iter().map(|s| (Occur::Should, s)).collect(),
        )),
    };

    // A single numeric type means block statistics can drive the scan. The query
    // itself decides per search whether that actually beats the general path.
    if types.len() == 1 && std::env::var("OBSEARCH_NO_BLOCK_RANGE").is_err() {
        if let Some(q) =
            block_range_query(ctx, &field, types[0], lower.as_ref(), upper.as_ref(), &general)
        {
            return Ok(q);
        }
    }
    Ok(general)
}

/// Encode a range bound into the column's monotonic u64 space.
///
/// Returns `None` whenever the bound cannot be represented exactly -- an
/// exclusive float bound, say -- so the caller falls back to the general path
/// rather than answering a slightly different question.
fn u64_bound(
    ty: Type,
    b: Option<&(Value, bool)>,
    is_lower: bool,
) -> Option<u64> {
    use tantivy::columnar::MonotonicallyMappableToU64;
    let Some((v, inclusive)) = b else {
        return Some(if is_lower { u64::MIN } else { u64::MAX });
    };
    let step = |x: u64| -> Option<u64> {
        if *inclusive {
            Some(x)
        } else if is_lower {
            x.checked_add(1)
        } else {
            x.checked_sub(1)
        }
    };
    match (ty, v) {
        (Type::I64, Value::Number(n)) => step(MonotonicallyMappableToU64::to_u64(n.as_i64()?)),
        (Type::U64, Value::Number(n)) => step(n.as_u64()?),
        (Type::F64, Value::Number(n)) => {
            // stepping a float bound would change which values qualify
            if !*inclusive {
                return None;
            }
            Some(MonotonicallyMappableToU64::to_u64(n.as_f64()?))
        }
        (Type::Date, Value::String(s)) => {
            let dt = parse_datetime(s)?;
            step(MonotonicallyMappableToU64::to_u64(dt))
        }
        _ => None,
    }
}

fn block_range_query(
    ctx: &Ctx,
    field: &str,
    ty: Type,
    lower: Option<&(Value, bool)>,
    upper: Option<&(Value, bool)>,
    general: &Box<dyn Query>,
) -> Option<Box<dyn Query>> {
    use tantivy::columnar::ColumnType;
    let column_type = match ty {
        Type::I64 => ColumnType::I64,
        Type::U64 => ColumnType::U64,
        Type::F64 => ColumnType::F64,
        Type::Date => ColumnType::DateTime,
        _ => return None,
    };
    let lo = u64_bound(ty, lower, true)?;
    let hi = u64_bound(ty, upper, false)?;
    if lo > hi {
        return Some(Box::new(EmptyQuery));
    }
    Some(Box::new(crate::blockstats::BlockRangeQuery {
        column: ctx.column_name(field, false),
        column_type,
        lo,
        hi,
        cache: ctx.stats.clone(),
        fallback: general.box_clone().into(),
    }))
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
        (Type::Bool, Value::Bool(b)) => t.append_type_and_fast_value(*b),
        (Type::Str, Value::String(s)) => t.append_type_and_str(s),
        (Type::Str, other) => t.append_type_and_str(&other.to_string()),
        _ => return Bound::Unbounded,
    }
    if *inclusive { Bound::Included(t) } else { Bound::Excluded(t) }
}


/// `match_bool_prefix`: every analysed term is a term query except the last,
/// which matches as a prefix.
/// `span_near` over ordered `span_term` clauses, optionally ending in a
/// `span_multi` prefix.
///
/// That shape is a phrase, which is what it is built as. The span family's
/// other members -- `span_or`, `span_not`, unordered clauses -- are not
/// expressible this way and are still refused rather than approximated.
fn build_span_near(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
    let clauses = body
        .get("clauses")
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow!("[span_near] requires [clauses]"))?;
    if body.get("in_order").and_then(|v| v.as_bool()) == Some(false) {
        return Err(anyhow!("unsupported query type [span_near] with in_order: false"));
    }
    let slop = body.get("slop").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

    let mut field: Option<String> = None;
    let mut words: Vec<String> = Vec::new();
    let mut prefix_last = false;
    for (i, clause) in clauses.iter().enumerate() {
        let (name, text, is_prefix) = if let Some(t) = clause.get("span_term") {
            let (f, v, _) = field_and_value(t)?;
            (f, v.as_str().unwrap_or_default().to_string(), false)
        } else if let Some(m) = clause.pointer("/span_multi/match/prefix") {
            let (f, v, _) = field_and_value(m)?;
            (f, v.as_str().unwrap_or_default().to_string(), true)
        } else {
            return Err(anyhow!("unsupported query type [span_near] clause"));
        };
        if is_prefix && i + 1 != clauses.len() {
            return Err(anyhow!("[span_multi] is only supported as the last clause"));
        }
        prefix_last |= is_prefix;
        match &field {
            Some(f) if *f != name => {
                return Err(anyhow!("[span_near] clauses must all name one field"));
            }
            _ => field = Some(name),
        }
        words.push(text);
    }
    let Some(field) = field else { return Ok(Box::new(EmptyQuery)) };
    let (f, path, view) = ctx.resolve(&field, true);

    let mut terms: Vec<Term> = Vec::new();
    for (i, w) in words.iter().enumerate() {
        let last = i + 1 == words.len();
        // the prefix clause is matched as written; the rest go through the
        // analyser so they meet the terms the field actually holds
        let pieces = if last && prefix_last {
            vec![if view == View::Dyn { w.to_lowercase() } else { w.clone() }]
        } else {
            analyze(ctx, view, w)
        };
        for p in pieces {
            let mut t = Term::from_field_json_path(f, &path, true);
            t.append_type_and_str(&p);
            terms.push(t);
        }
    }
    if terms.is_empty() {
        return Ok(Box::new(EmptyQuery));
    }
    if prefix_last {
        let mut q = tantivy::query::PhrasePrefixQuery::new(terms);
        q.set_max_expansions(50);
        return Ok(Box::new(q));
    }
    if terms.len() == 1 {
        return Ok(Box::new(TermQuery::new(terms.remove(0), IndexRecordOption::WithFreqs)));
    }
    let mut q = PhraseQuery::new(terms);
    q.set_slop(slop);
    Ok(Box::new(q))
}

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

/// Lower a pattern's literal letters, leaving escapes alone -- `\\W` is not
/// `\\w`.
fn lowercase_regex(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut escaped = false;
    for c in pattern.chars() {
        if escaped {
            out.push(c);
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            out.push(c);
            continue;
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// Widen every cased letter of a pattern into a two-way character class, so a
/// literal can be matched without regard to case.
fn case_insensitive_regex(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut escaped = false;
    let mut in_class = false;
    for c in pattern.chars() {
        if !escaped {
            if c == '[' {
                in_class = true;
            } else if c == ']' {
                in_class = false;
            }
        }
        // inside a character class an expansion would nest brackets
        if escaped || in_class || !c.is_alphabetic() {
            out.push(c);
            escaped = !escaped && c == '\\';
            continue;
        }
        let (lo, up): (String, String) =
            (c.to_lowercase().collect(), c.to_uppercase().collect());
        if lo == up {
            out.push(c);
        } else {
            out.push_str(&format!("[{lo}{up}]"));
        }
    }
    out
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
        let value = if value.starts_with('[') || value.starts_with('{') {
            value
        } else {
            value.trim_matches('"').to_string()
        };
        if value.is_empty() {
            continue;
        }
        let regex_literal = value.len() > 2 && value.starts_with('/') && value.ends_with('/');
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
/// tantivy has one of these already, but its weight leaves `for_each_pruning`
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
    score: tantivy::Score,
}

impl ConstScore {
    pub fn new(query: Box<dyn Query>, score: tantivy::Score) -> Self {
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
        ConstScore {
            query: self.query.box_clone(),
            score: self.score,
        }
    }
}

impl Query for ConstScore {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
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
    score: tantivy::Score,
}

impl Weight for ConstWeight {
    fn scorer(
        &self,
        reader: &tantivy::SegmentReader,
        boost: tantivy::Score,
    ) -> tantivy::Result<Box<dyn tantivy::query::Scorer>> {
        let inner = self.inner.scorer(reader, boost)?;
        Ok(Box::new(tantivy::query::ConstScorer::new(
            inner,
            boost * self.score,
        )))
    }

    fn explain(
        &self,
        reader: &tantivy::SegmentReader,
        doc: tantivy::DocId,
    ) -> tantivy::Result<tantivy::query::Explanation> {
        let mut ex = tantivy::query::Explanation::new("Const", self.score);
        ex.add_detail(self.inner.explain(reader, doc)?);
        Ok(ex)
    }

    fn count(&self, reader: &tantivy::SegmentReader) -> tantivy::Result<u32> {
        self.inner.count(reader)
    }

    fn for_each_pruning(
        &self,
        threshold: tantivy::Score,
        reader: &tantivy::SegmentReader,
        callback: &mut dyn FnMut(tantivy::DocId, tantivy::Score) -> tantivy::Score,
    ) -> tantivy::Result<()> {
        use tantivy::DocSet;
        // nothing here can beat what the collector already holds
        if threshold >= self.score {
            return Ok(());
        }
        // the inner scorer is walked for its documents alone; the score it
        // would compute is discarded, so ask for the cheaper unscored form
        let mut scorer = self.inner.scorer(reader, 1.0)?;
        let mut doc = scorer.doc();
        while doc != tantivy::TERMINATED {
            if callback(doc, self.score) >= self.score {
                return Ok(());
            }
            doc = scorer.advance();
        }
        Ok(())
    }
}
