//! OpenSearch query DSL -> BoostCore queries.

use crate::store::{Fields, Mapping};
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::ops::Bound;
use std::sync::Arc;
use boostcore::query::{
    AllQuery, AutomatonWeight, BooleanQuery, BoostQuery, EmptyQuery, EnableScoring, ExistsQuery,
    FuzzyTermQuery, Occur, PhraseQuery, Query, RangeQuery, TermQuery, Weight,
};
use boostcore::schema::{Field, IndexRecordOption, Term, Type};
use boostcore::{Index, TantivyError};
use boostcore_fst::Regex;

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
            // every document matches, and each one equally: a score of one
            let base: Box<dyn Query> = Box::new(ConstScore::new(Box::new(AllQuery), 1.0));
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
            // the values gathered under a flat_object keep the spelling and
            // the type they were stored with, whether the query names the
            // object itself or a path inside it
            let under_flat = {
                let mut walked = String::new();
                let mut found = false;
                for part in field.split('.') {
                    walked = if walked.is_empty() {
                        part.to_string()
                    } else {
                        format!("{walked}.{part}")
                    };
                    if ctx.mapping.type_of(&walked) == Some("flat_object") {
                        found = true;
                    }
                }
                found
            };
            if under_flat {
                if let Some(text) = val.as_str() {
                    let mut terms = term_for(f, &path, &val);
                    let normal = normalized(ctx, &field, text);
                    if normal != text {
                        terms.extend(term_for(f, &path, &Value::String(normal)));
                    }
                    if let Some(iso) =
                        crate::store::canonical_date(&Value::String(text.to_string()))
                    {
                        if iso != text {
                            terms.extend(term_for(f, &path, &Value::String(iso)));
                        }
                    }
                    // a number gathered under a flat_object is still a number
                    if let Ok(n) = text.parse::<f64>() {
                        if let Some(num) = serde_json::Number::from_f64(n) {
                            terms.extend(term_for(f, &path, &Value::Number(num)));
                        }
                    }
                    // the values are text like any other, and score like it
                    return Ok(any_of(terms));
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
            // MappedFieldType.termsQuery builds "a constant-scoring query that
            // matches all values": matching two of the terms says no more about
            // a document than matching one, so the order falls back to doc id
            let inner: Box<dyn Query> = if subs.is_empty() {
                any_of(terms)
            } else {
                if !terms.is_empty() {
                    subs.push(any_of(terms));
                }
                Box::new(BooleanQuery::new(
                    subs.into_iter().map(|q| (Occur::Should, q)).collect(),
                ))
            };
            Box::new(ConstScore::new(inner, 1.0))
        }
        "ids" => {
            let arr = body.get("values").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            let terms: Vec<Term> = arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| Term::from_field_text(ctx.fields.id, s))
                .collect();
            Box::new(ConstScore::new(any_of(terms), 1.0))
        }
        "exists" => {
            let field = body.get("field").and_then(|f| f.as_str()).unwrap_or_default();
            // every document has an id and belongs to an index, so asking
            // whether one exists is asking for all of them
            // `_source` is not a field to ask after: it is the document
            if field == "_source" {
                return Err(anyhow!(
                    "query_shard_exception: Cannot search on field [_source] since it is not \
                     indexed."
                ));
            }
            // every document has an id, an index and a sequence number
            if field == "_id" || field == "_index" || field == "_seq_no" || field == "_version" {
                return Ok(Box::new(AllQuery));
            }
            let col = ctx.column_name(field, false);
            Box::new(ExistsQuery::new(col, true))
        }
        // a shape, a box or a radius all ask where a point is; the field has
        // to be there and the answer is worked out once the candidates are
        // known
        "geo_shape" | "geo_bounding_box" | "geo_distance" | "geo_polygon" => {
            let field = body
                .as_object()
                .and_then(|o| {
                    o.keys()
                        .map(|k| k.to_string())
                        .find(|k| !matches!(k.as_str(), "boost" | "_name" | "ignore_unmapped"
                            | "validation_method" | "type" | "distance" | "distance_type"
                            | "relation"))
                })
                .unwrap_or_default();
            let col = ctx.column_name(&field, false);
            Box::new(ExistsQuery::new(col, true))
        }
        // `distance_feature` ranks by how near a value is to an origin; every
        // document that has the field takes part, and the ranking itself is
        // worked out once the candidates are known
        "distance_feature" => {
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
            // a long pattern costs what it costs to run against every term, so
            // the index says how long one may be
            let n = val.as_str().map(|s| s.chars().count()).unwrap_or(0);
            if n > ctx.max_regex_length {
                return Err(anyhow!(
                    "The length of regex [{n}] used in the Regexp Query request has exceeded the \
                     allowed maximum of [{}]. This maximum can be set by changing the \
                     [index.max_regex_length] index level setting.",
                    ctx.max_regex_length
                ));
            }
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
        // documents here are stored whole rather than split into a parent and
        // its nested children, so a nested query is its inner query asked
        // against the same document
        "nested" => {
            let inner = body
                .get("query")
                .ok_or_else(|| anyhow!("[nested] requires 'query' field"))?;
            build(ctx, inner)?
        }
        // an `intervals` query is a little language of rules over one field.
        // Positions are not compared here; each rule is built as the query it
        // most nearly is, and the shape of the rule tree is kept.
        "intervals" => {
            let Some((field, rule)) = body.as_object().and_then(|o| o.iter().next()) else {
                return Err(anyhow!("[intervals] requires a field"));
            };
            build_interval_rule(ctx, field, rule)?
        }
        // `terms_set` asks for a number of the listed terms rather than all
        // of them, and how many is read from a field of the document itself
        "terms_set" => {
            let Some((field, spec)) = body.as_object().and_then(|o| o.iter().next()) else {
                return Err(anyhow!("[terms_set] requires a field"));
            };
            let terms: Vec<Value> = spec
                .get("terms")
                .and_then(|t| t.as_array())
                .cloned()
                .unwrap_or_default();
            let clauses: Vec<Value> = terms
                .iter()
                .map(|t| serde_json::json!({"term": {field.clone(): t.clone()}}))
                .collect();
            // without a count to read, every term is required
            let mut inner = serde_json::json!({"bool": {"should": clauses}});
            if spec.get("minimum_should_match_field").is_some()
                || spec.get("minimum_should_match_script").is_some()
            {
                // how many are needed is a property of each document, which
                // this engine cannot ask of a scorer; one is the floor
                inner["bool"]["minimum_should_match"] = serde_json::json!(1);
            } else if let Some(n) = spec.get("minimum_should_match") {
                inner["bool"]["minimum_should_match"] = n.clone();
            }
            build(ctx, &inner)?
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
            Box::new(boostcore::query::DisjunctionMaxQuery::new(subs?))
        }
                other => {
            // a near-miss is usually a typo, and saying which name was meant
            // saves the caller reading the whole list
            const KNOWN: &[&str] = &[
                "bool", "term", "terms", "match", "match_all", "match_none", "range",
                "prefix", "wildcard", "regexp", "fuzzy", "exists", "ids", "nested",
                "match_phrase", "multi_match", "query_string", "simple_query_string",
                "constant_score", "dis_max", "boosting", "function_score", "more_like_this",
            ];
            let near = KNOWN.iter().find(|k| {
                k.len().abs_diff(other.len()) <= 2
                    && k.chars().zip(other.chars()).filter(|(a, b)| a == b).count() + 2
                        >= k.len()
            });
            return Err(match near {
                Some(k) => anyhow!("unknown query [{other}] did you mean [{k}]?"),
                None => anyhow!("unknown query [{other}]"),
            });
        }
    };

    // a boost sits beside the clause, or -- where a clause names one field --
    // beside that field's own options
    let clause = q.get(&kind);
    // some queries walk the whole term dictionary, and a cluster may say it
    // would rather not
    if !ctx.allow_expensive {
        let tail = match kind.as_str() {
            "prefix" => Some(" For optimised prefix queries on text fields please enable \
                              [index_prefixes]."),
            "fuzzy" | "regexp" | "wildcard" => Some(""),
            _ => None,
        };
        if let Some(tail) = tail {
            return Err(anyhow!(
                "[{kind}] queries cannot be executed when 'search.allow_expensive_queries' is \
                 set to false.{tail}"
            ));
        }
        // a range over text is a walk of the dictionary too; over a number it
        // is not
        if kind == "range" {
            let field = q
                .get(&kind)
                .and_then(|b| b.as_object())
                .and_then(|o| o.keys().next().cloned())
                .unwrap_or_default();
            if matches!(
                ctx.mapping.type_of(&field),
                Some("text") | Some("keyword") | Some("match_only_text")
            ) {
                return Err(anyhow!(
                    "[range] queries on [text] or [keyword] fields cannot be executed when \
                     'search.allow_expensive_queries' is set to false."
                ));
            }
        }
        if kind == "nested" || kind == "has_child" || kind == "has_parent" {
            return Err(anyhow!(
                "[joining] queries cannot be executed when 'search.allow_expensive_queries' is \
                 set to false."
            ));
        }
    }
    let boost = clause
        .and_then(|b| b.get("boost"))
        .or_else(|| {
            clause
                .and_then(|b| b.as_object())
                .filter(|o| o.len() == 1)
                .and_then(|o| o.values().next())
                .and_then(|v| v.get("boost"))
        })
        .and_then(|b| b.as_f64())
        .filter(|_| kind != "match_all" && kind != "constant_score");
    Ok(match boost {
        Some(b) => Box::new(BoostQuery::new(inner, b as f32)),
        None => inner,
    })
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
            _ => Box::new(BooleanQuery::union(per_field))
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
        ConstScore {
            query: self.query.box_clone(),
            score: self.score,
        }
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
        Ok(Box::new(boostcore::query::ConstScorer::new(
            inner,
            boost * self.score,
        )))
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
                    tokens
                        .iter()
                        .enumerate()
                        .filter(|(_, t)| *t == w)
                        .map(|(i, _)| i)
                        .collect()
                })
                .collect();
            combine_spans(&places.iter().map(|p| p.iter().map(|i| (*i, *i)).collect()).collect::<Vec<Vec<Span>>>(), ordered, no_overlap)
        }
        "prefix" => {
            let want = spec.get("prefix").and_then(|v| v.as_str()).unwrap_or_default().to_lowercase();
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
            let want =
                spec.get("term").and_then(|v| v.as_str()).unwrap_or_default().to_lowercase();
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
    tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| hit(t))
        .map(|(i, _)| (i, i))
        .collect()
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
