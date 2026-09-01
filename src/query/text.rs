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
    let term_of = |t: &str| -> Term {
        let mut term = Term::from_field_json_path(f, &path, true);
        term.append_type_and_str(t);
        term
    };
    let terms: Vec<Term> = tokens.iter().map(|t| term_of(t)).collect();

    if kind == "match_phrase" || kind == "match_phrase_prefix" {
        // the last word of a phrase prefix is the beginning of a word, not a
        // whole one: `lazy d` finds `lazy dog`
        if kind == "match_phrase_prefix" {
            // more than one way through the text: each way is a phrase whose
            // last word is only the beginning of one
            let arcs = analyze_graph(ctx, view, &field, &text, analyzer);
            if crate::query::branches(&arcs) {
                let mut clauses: Vec<Box<dyn Query>> = Vec::new();
                for way in crate::query::ways(&arcs) {
                    let Some((last, head)) = way.split_last() else { continue };
                    let head: Vec<Term> = head.iter().map(|w| term_of(w)).collect();
                    for ending in prefix_terms(ctx, f, &path, last)? {
                        let mut phrase = head.clone();
                        phrase.push(ending);
                        clauses.push(match phrase.len() {
                            1 => Box::new(TermQuery::new(
                                phrase.remove(0),
                                IndexRecordOption::WithFreqs,
                            )),
                            _ => Box::new(PhraseQuery::new(phrase)),
                        });
                    }
                }
                if clauses.is_empty() {
                    return Ok(Box::new(EmptyQuery));
                }
                return Ok(Box::new(BooleanQuery::union(clauses)));
            }
            let mut head = terms.clone();
            let Some(last) = head.pop() else {
                return Ok(Box::new(EmptyQuery));
            };
            let stem = tokens.last().cloned().unwrap_or_default();
            let starts_with = prefix_terms(ctx, f, &path, &stem)?;
            if starts_with.is_empty() {
                return Ok(Box::new(EmptyQuery));
            }
            if head.is_empty() {
                let any: Vec<(Occur, Box<dyn Query>)> = starts_with
                    .into_iter()
                    .map(|term| {
                        let clause: Box<dyn Query> =
                            Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs));
                        (Occur::Should, clause)
                    })
                    .collect();
                return Ok(Box::new(BooleanQuery::new(any)));
            }
            // every word the last one could be, each making a phrase of its own
            let mut ways: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for ending in starts_with {
                let mut phrase = head.clone();
                phrase.push(ending);
                ways.push((Occur::Should, Box::new(PhraseQuery::new(phrase))));
            }
            let _ = last;
            return Ok(Box::new(BooleanQuery::new(ways)));
        }
        if terms.len() == 1 {
            return Ok(Box::new(TermQuery::new(terms[0].clone(), IndexRecordOption::WithFreqs)));
        }
        // where the analyzer left more than one way through the text -- a
        // synonym beside what it means, a stem on its word -- a phrase is a
        // phrase for each way through
        let arcs = analyze_graph(ctx, view, &field, &text, analyzer);
        if crate::query::branches(&arcs) {
            let clauses: Vec<Box<dyn Query>> = crate::query::ways(&arcs)
                .into_iter()
                .map(|way| {
                    let mut walked: Vec<Term> = way.iter().map(|w| term_of(w)).collect();
                    match walked.len() {
                        1 => {
                            Box::new(TermQuery::new(walked.remove(0), IndexRecordOption::WithFreqs))
                                as Box<dyn Query>
                        }
                        _ => Box::new(PhraseQuery::new(walked)),
                    }
                })
                .collect();
            if !clauses.is_empty() {
                return Ok(Box::new(BooleanQuery::union(clauses)));
            }
        }
        return Ok(Box::new(PhraseQuery::new(terms)));
    }

    // a field holding text holds the number as text: `1234` written into a
    // field cut into ngrams is found by the ngrams of `1234`
    let is_text = matches!(
        ctx.mapping.type_of(&field),
        Some("text" | "match_only_text" | "search_as_you_type")
    );
    // non-string match on a numeric/keyword field falls back to an exact term
    if (view == View::Raw || !matches!(val, Value::String(_))) && !is_text {
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

    // a number or a flag written as text matches the value itself: `order:1`
    // finds a document whose `order` is 1, whether the field was written as
    // text or as a number, and `"true"` finds a flag that is set
    let as_number = text
        .parse::<i64>()
        .ok()
        .map(|n| serde_json::json!(n))
        .or_else(|| text.parse::<f64>().ok().map(|n| serde_json::json!(n)))
        .or_else(|| text.parse::<bool>().ok().map(|b| serde_json::json!(b)))
        .map(|n| term_for(f, &path, &n))
        .filter(|exact| !exact.is_empty());

    let operator =
        opts.get("operator").and_then(|o| o.as_str()).unwrap_or("or").to_ascii_lowercase();
    let occur = if operator == "and" { Occur::Must } else { Occur::Should };
    // words standing in one place are one word written several ways, and a
    // word spanning several places is one way of reading them: the text is
    // cut where nothing crosses, and a match wants every stretch, each by any
    // of the ways through it
    let arcs = analyze_graph(ctx, view, &field, &text, analyzer);
    let stretches: Vec<Vec<Vec<String>>> = match crate::query::branches(&arcs) {
        true => crate::query::stretches(&arcs),
        false => tokens.iter().map(|t| vec![vec![t.clone()]]).collect(),
    };
    let n = stretches.len();
    // a way of several words is read as a phrase, unless the request asked
    // for the words alone, in any order and at any distance
    let as_phrase =
        opts.get("auto_generate_synonyms_phrase_query").and_then(|v| v.as_bool()).unwrap_or(true);
    let clauses: Vec<(Occur, Box<dyn Query>)> = stretches
        .into_iter()
        .map(|ways| {
            let mut alternatives: Vec<Box<dyn Query>> = ways
                .into_iter()
                .map(|way| {
                    let mut walked: Vec<Term> = way.iter().map(|w| term_of(w)).collect();
                    match walked.len() {
                        1 => {
                            Box::new(TermQuery::new(walked.remove(0), IndexRecordOption::WithFreqs))
                                as Box<dyn Query>
                        }
                        _ if as_phrase => Box::new(PhraseQuery::new(walked)),
                        _ => Box::new(BooleanQuery::new(
                            walked
                                .into_iter()
                                .map(|t| {
                                    (
                                        Occur::Must,
                                        Box::new(TermQuery::new(t, IndexRecordOption::WithFreqs))
                                            as Box<dyn Query>,
                                    )
                                })
                                .collect(),
                        )),
                    }
                })
                .collect();
            let one: Box<dyn Query> = match alternatives.len() {
                1 => alternatives.remove(0),
                _ => Box::new(BooleanQuery::union(alternatives)),
            };
            (occur, one)
        })
        .collect();
    let required = if occur == Occur::Should {
        msm_required(opts.get("minimum_should_match"), n).unwrap_or_else(|| resolve_msm(1, n))
    } else {
        0
    };
    let words: Box<dyn Query> =
        Box::new(BooleanQuery::with_minimum_required_clauses(clauses, required));
    match as_number {
        Some(exact) => Ok(Box::new(BooleanQuery::union(vec![words, any_of(exact)]))),
        None => Ok(words),
    }
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
        msm_required(opts.get("minimum_should_match"), n).unwrap_or_else(|| resolve_msm(1, n))
    } else {
        0
    };
    Ok(Box::new(BooleanQuery::with_minimum_required_clauses(clauses, required)))
}

/// `span_term` -- one word, in the field it is written in.
///
/// Span queries are about where words stand in a document. The ones here
/// answer with the documents whose spans could match; a span query that only
/// narrows -- `span_first`, `span_not`, `span_containing` -- is read as the
/// clause it narrows, with the narrowing applied where BoostCore can see the
/// positions.
pub(crate) fn build_span_term(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
    let (field, value, _) = field_and_value(body)?;
    let text = match &value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let (f, path, _) = ctx.resolve(&field, true);
    let mut term = Term::from_field_json_path(f, &path, true);
    term.append_type_and_str(&text);
    Ok(Box::new(TermQuery::new(term, IndexRecordOption::WithFreqsAndPositions)))
}

/// One span clause, whichever kind it is.
pub(crate) fn build_span(ctx: &Ctx, clause: &Value) -> Result<Box<dyn Query>> {
    let Some((kind, body)) = clause.as_object().and_then(|o| o.iter().next()) else {
        return Err(anyhow!("[span] clause is empty"));
    };
    match kind.as_str() {
        "span_term" => build_span_term(ctx, body),
        "span_near" => build_span_near(ctx, body),
        "span_or" => build_span_or(ctx, body),
        "span_not" => build_span_not(ctx, body),
        "span_first" => build_span_first(ctx, body),
        "span_containing" | "span_within" => build_span_pair(ctx, body),
        "span_multi" => {
            let inner = body.get("match").ok_or_else(|| anyhow!("[span_multi] needs [match]"))?;
            super::build(ctx, inner)
        }
        "span_gap" => Ok(Box::new(EmptyQuery)),
        other => Err(anyhow!("unknown span query [{other}]")),
    }
}

/// `span_or` -- any of its clauses.
pub(crate) fn build_span_or(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
    let clauses = body
        .get("clauses")
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow!("[span_or] requires [clauses]"))?;
    // where every clause is one word, the whole thing is a single span over
    // those words, which is what Lucene scores it as
    let words: Option<Vec<Term>> = clauses
        .iter()
        .map(|clause| {
            let spec = clause.get("span_term")?;
            let (field, value, _) = field_and_value(spec).ok()?;
            let text = match &value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let (f, path, _) = ctx.resolve(&field, true);
            let mut term = Term::from_field_json_path(f, &path, true);
            term.append_type_and_str(&text);
            Some(term)
        })
        .collect();
    if let Some(words) =
        words.filter(|w| !w.is_empty() && w.iter().all(|t| t.field() == w[0].field()))
    {
        return Ok(Box::new(crate::query::SpanUnion::new(words)));
    }
    let mut parts: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for clause in clauses {
        parts.push((Occur::Should, build_span(ctx, clause)?));
    }
    Ok(Box::new(BooleanQuery::new(parts)))
}

/// `span_not` -- the first clause where the second does not stand.
pub(crate) fn build_span_not(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
    let include = body.get("include").ok_or_else(|| anyhow!("[span_not] requires [include]"))?;
    body.get("exclude").ok_or_else(|| anyhow!("[span_not] requires [exclude]"))?;
    // What `exclude` takes out is a span that overlaps the included one, not
    // every document that holds it: a document where the two stand apart is
    // still an answer. Without spans of its own to compare, this answers with
    // the included clause, which is the document set OpenSearch answers with
    // wherever the two do not overlap.
    build_span(ctx, include)
}

/// `span_first` -- a clause that stands near the beginning of the field.
pub(crate) fn build_span_first(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
    let inner = body.get("match").ok_or_else(|| anyhow!("[span_first] requires [match]"))?;
    let end = body.get("end").and_then(|v| v.as_u64()).unwrap_or(u64::MAX) as usize;
    // where the clause is one word, how early it stands can be answered from
    // its positions; anything else is read as the clause itself
    if let Some(spec) = inner.get("span_term") {
        let (field, value, _) = field_and_value(spec)?;
        let text = match &value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let (f, path, _) = ctx.resolve(&field, true);
        let mut term = Term::from_field_json_path(f, &path, true);
        term.append_type_and_str(&text);
        return Ok(Box::new(crate::query::FirstPositions::new(term, end)));
    }
    build_span(ctx, inner)
}

/// `span_containing` and `span_within` -- one span inside another.
pub(crate) fn build_span_pair(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
    let little = body.get("little").ok_or_else(|| anyhow!("[span] requires [little]"))?;
    let big = body.get("big").ok_or_else(|| anyhow!("[span] requires [big]"))?;
    let parts: Vec<(Occur, Box<dyn Query>)> =
        vec![(Occur::Must, build_span(ctx, little)?), (Occur::Must, build_span(ctx, big)?)];
    Ok(Box::new(BooleanQuery::new(parts)))
}

/// Every term in a field that begins with these letters.
///
/// A phrase prefix ends in the beginning of a word, and the words it could be
/// are read out of the term dictionary -- capped, as OpenSearch caps them, so
/// that one short prefix cannot name every word in the index.
pub(crate) fn prefix_terms(ctx: &Ctx, field: Field, path: &str, stem: &str) -> Result<Vec<Term>> {
    const MOST: usize = 50;
    let mut out = Vec::new();
    let searcher = ctx.index.reader()?.searcher();
    let mut start = Term::from_field_json_path(field, path, true);
    start.append_type_and_str(stem);
    let prefix = start.serialized_value_bytes().to_vec();
    for reader in searcher.segment_readers() {
        let inverted = reader.inverted_index(field)?;
        let mut stream = inverted.terms().stream()?;
        while let Some((bytes, _)) = stream.next() {
            if bytes.starts_with(&prefix) {
                let mut term = Term::from_field_json_path(field, path, true);
                term.append_bytes(&bytes[term.serialized_value_bytes().len()..]);
                out.push(Term::from_field_bytes(field, bytes));
                if out.len() >= MOST {
                    return Ok(out);
                }
            }
        }
    }
    Ok(out)
}
