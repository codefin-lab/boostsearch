//! The queries that match text: what the words are, in what order, how near.

use super::*;

/// One rule of an `intervals` query, as the query it most nearly is.
pub(crate) fn build_interval_rule(ctx: &Ctx, field: &str, rule: &Value) -> Result<Box<dyn Query>> {
    let Some((kind, spec)) = rule.as_object().and_then(|o| o.iter().next()) else {
        return Err(anyhow!("[intervals] requires a rule"));
    };
    // a rule may name a different field to read
    let field = spec
        .get("use_field")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| field.to_string());
    let insensitive = is_true(spec.get("case_insensitive"));
    let clause = match kind.as_str() {
        "match" => {
            let text = spec.get("query").cloned().unwrap_or(Value::Null);
            let ordered = is_true(spec.get("ordered"));
            let gaps = spec.get("max_gaps").and_then(|v| v.as_i64()).unwrap_or(-1);
            let words = text.as_str().unwrap_or_default().split_whitespace().count();
            let inner = if ordered && gaps == 0 {
                serde_json::json!({"match_phrase": {field.clone(): {"query": text}}})
            } else if ordered && words > 1 {
                // the words have to turn up in the order they were written,
                // with as much between them as the rule allows
                let clauses: Vec<Value> = text
                    .as_str()
                    .unwrap_or_default()
                    .split_whitespace()
                    .map(|w| serde_json::json!({"span_term": {field.clone(): w}}))
                    .collect();
                let slop = if gaps < 0 { 1_000 } else { gaps as u64 };
                serde_json::json!({
                    "span_near": {"clauses": clauses, "slop": slop, "in_order": true}
                })
            } else {
                serde_json::json!({"match": {field.clone(): {"query": text, "operator": "and"}}})
            };
            build(ctx, &inner)?
        }
        "prefix" => {
            let text = spec.get("prefix").cloned().unwrap_or(Value::Null);
            build(ctx, &serde_json::json!({"prefix": {field.clone(): text}}))?
        }
        "wildcard" => {
            let text = spec.get("pattern").cloned().unwrap_or(Value::Null);
            build(ctx, &serde_json::json!({"wildcard": {field.clone(): text}}))?
        }
        "fuzzy" => {
            let text = spec.get("term").cloned().unwrap_or(Value::Null);
            build(ctx, &serde_json::json!({"fuzzy": {field.clone(): {"value": text}}}))?
        }
        "regexp" => {
            let text = spec.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let mut inner = serde_json::json!({"regexp": {field.clone(): {"value": text}}});
            if insensitive {
                inner["regexp"][field.clone()]["case_insensitive"] = serde_json::json!(true);
            }
            build(ctx, &inner)?
        }
        "all_of" | "any_of" => {
            let occur = if kind == "all_of" { Occur::Must } else { Occur::Should };
            let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for sub in spec.get("intervals").and_then(|v| v.as_array()).into_iter().flatten() {
                clauses.push((occur, build_interval_rule(ctx, &field, sub)?));
            }
            if clauses.is_empty() {
                return Ok(Box::new(EmptyQuery));
            }
            Box::new(BooleanQuery::new(clauses))
        }
        other => return Err(anyhow!("Unknown interval rule [{other}]")),
    };
    // `filter` narrows what the rule matched; the parts of it this engine can
    // answer are the ones that name a query
    let filtered = spec.get("filter").and_then(|f| f.as_object()).map(|f| {
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for (name, inner) in f {
            let occur = match name.as_str() {
                "not_contained_by" | "not_containing" | "not_overlapping" => Occur::MustNot,
                "filter" => Occur::Must,
                _ => Occur::Must,
            };
            if let Ok(q) = build_interval_rule(ctx, &field, inner) {
                clauses.push((occur, q));
            }
        }
        clauses
    });
    match filtered {
        Some(mut clauses) if !clauses.is_empty() => {
            clauses.insert(0, (Occur::Must, clause));
            Ok(Box::new(BooleanQuery::new(clauses)))
        }
        _ => Ok(clause),
    }
}

pub(crate) fn build_match(ctx: &Ctx, kind: &str, body: &Value) -> Result<Box<dyn Query>> {
    let (field, val, opts) = field_and_value(body)?;
    let (f, path, view) = ctx.resolve(&field, true);
    let text = match &val {
        Value::String(s) => s.clone(),
        other => other.to_string().trim_matches('"').to_string(),
    };
    let analyzer = opts.get("analyzer").and_then(|v| v.as_str());
    let tokens = analyze_with(ctx, view, &field, &text, analyzer);
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
        // matching a number is matching one value, not a passage of text: it
        // is either there or it is not, and every document that has it is
        // equally a match
        if !matches!(val, Value::String(_)) {
            return Ok(Box::new(ConstScore::new(any_of(exact), 1.0)));
        }
        return Ok(any_of(exact));
    }

    let operator =
        opts.get("operator").and_then(|o| o.as_str()).unwrap_or("or").to_ascii_lowercase();
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

pub(crate) fn build_multi_match(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
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
        let clause = Value::Object([(name.to_string(), Value::Object(per))].into_iter().collect());
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
    // one clause is the query; none is a query that matches nothing
    let mut subs = subs;
    match subs.len() {
        0 => return Ok(Box::new(EmptyQuery)),
        1 => return Ok(subs.remove(0)),
        _ => {}
    }
    // most_fields/cross_fields sum the per-field scores; best_fields takes the best
    if kind == "most_fields" || kind == "cross_fields" || kind == "bool_prefix" {
        Ok(Box::new(BooleanQuery::union(subs)))
    } else {
        Ok(Box::new(boostcore::query::DisjunctionMaxQuery::new(subs)))
    }
}

/// `match_bool_prefix`: every analysed term is a term query except the last,
/// which matches as a prefix.
/// `span_near` over ordered `span_term` clauses, optionally ending in a
/// `span_multi` prefix.
///
/// That shape is a phrase, which is what it is built as. The span family's
/// other members -- `span_or`, `span_not`, unordered clauses -- are not
/// expressible this way and are still refused rather than approximated.
pub(crate) fn build_span_near(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
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
            analyze(ctx, view, &field, w)
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
        let mut q = boostcore::query::PhrasePrefixQuery::new(terms);
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

pub(crate) fn build_match_bool_prefix(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
    let (field, val, opts) = field_and_value(body)?;
    for banned in ["slop", "cutoff_frequency"] {
        if opts.get(banned).is_some() {
            return Err(anyhow!("[{banned}] not allowed for type [bool_prefix]"));
        }
    }
    let (f, path, view) = ctx.resolve(&field, true);
    let text = val.as_str().unwrap_or_default();
    let analyzer = opts.get("analyzer").and_then(|v| v.as_str());
    let tokens = analyze_with(ctx, view, &field, text, analyzer);
    if tokens.is_empty() {
        return Ok(Box::new(EmptyQuery));
    }
    let operator =
        opts.get("operator").and_then(|o| o.as_str()).unwrap_or("or").to_ascii_lowercase();
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
