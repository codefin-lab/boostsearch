//! The queries that match text: what the words are, in what order, how near.

use super::*;

pub(crate) fn build_match(ctx: &Ctx, kind: &str, body: &Value) -> Result<Box<dyn Query>> {
    let (field, val, opts) = field_and_value(body)?;
    // a date, a number, a boolean or an address is one value, not text to
    // cut into words: matching it is asking for that value
    if kind == "match"
        && matches!(
            ctx.mapping.type_of(&field),
            Some(
                "date"
                    | "date_nanos"
                    | "long"
                    | "integer"
                    | "short"
                    | "byte"
                    | "double"
                    | "float"
                    | "half_float"
                    | "scaled_float"
                    | "unsigned_long"
                    | "boolean"
                    | "ip"
            )
        )
        && !val.is_null()
    {
        // a value is there or not: the score is one; the boost the clause
        // carries is applied by the caller, as for every clause
        let spec = serde_json::json!({ field.clone(): { "value": val.clone() } });
        return Ok(Box::new(ConstScore::new(
            build(ctx, &serde_json::json!({ "term": spec }))?,
            1.0,
        )));
    }
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
                let mut every: Vec<Term> = Vec::new();
                for way in crate::query::ways(&arcs) {
                    let Some((last, head)) = way.split_last() else { continue };
                    let head: Vec<Term> = head.iter().map(|w| term_of(w)).collect();
                    for ending in prefix_terms(ctx, f, &path, last)? {
                        let mut phrase = head.clone();
                        phrase.push(ending);
                        every.extend(phrase.iter().cloned());
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
                return Ok(Box::new(crate::query::SpanPaths::new(every, clauses)));
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
            let mut every: Vec<Term> = Vec::new();
            let clauses: Vec<Box<dyn Query>> = crate::query::ways(&arcs)
                .into_iter()
                .map(|way| {
                    let mut walked: Vec<Term> = way.iter().map(|w| term_of(w)).collect();
                    every.extend(walked.iter().cloned());
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
                return Ok(Box::new(crate::query::SpanPaths::new(every, clauses)));
            }
        }
        // Each word keeps the place the analyzer gave it. A stop word the
        // analyzer dropped leaves a gap -- `notify the supplier` cut by the
        // english analyzer is `notifi` and `supplier` two places apart -- and
        // the document was written with the same gap, so a phrase that closed
        // it up found nothing where the reference finds the clause.
        let offsets: Vec<usize> = match arcs.len() == terms.len() {
            true => arcs.iter().map(|a| a.from.saturating_sub(arcs[0].from)).collect(),
            false => (0..terms.len()).collect(),
        };
        // `slop` lets the words stand that many moves apart. It was read by
        // `span_near` and nowhere here, so `match_phrase: {query: "quick
        // fox", slop: 2}` found nothing in "quick brown fox".
        // With room between the words, each match counts by how far the
        // words moved to make it, which BoostCore's phrase does not weigh.
        let slop = opts.get("slop").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        if slop > 0 {
            return Ok(Box::new(crate::query::SloppyPhrase::new(terms, offsets, slop)));
        }
        return Ok(Box::new(PhraseQuery::new_with_offset(
            offsets.into_iter().zip(terms).collect(),
        )));
    }

    // a field holding text holds the number as text: `1234` written into a
    // field cut into ngrams is found by the ngrams of `1234`
    let is_text = matches!(
        ctx.mapping.type_of(&field),
        Some("text" | "match_only_text" | "search_as_you_type" | "annotated_text")
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
    // a field that keeps neither frequencies nor norms scores a word as
    // merely there, once, in a field of no particular length
    let flat = ctx.mapping.type_of(&field) == Some("match_only_text");
    let clauses: Vec<(Occur, Box<dyn Query>)> = stretches
        .into_iter()
        .map(|ways| {
            let mut alternatives: Vec<Box<dyn Query>> = ways
                .into_iter()
                .map(|way| {
                    let mut walked: Vec<Term> = way.iter().map(|w| term_of(w)).collect();
                    match walked.len() {
                        1 if flat => Box::new(crate::query::SpanUnion::flat(walked.remove(0)))
                            as Box<dyn Query>,
                        // `fuzziness` lets a word stand for the words that
                        // many edits away from it. It was read by other
                        // queries and not by `match`, so `quikc` with
                        // `fuzziness: AUTO` found nothing at all.
                        1 => match fuzzy_edits(opts.get("fuzziness"), &way[0]) {
                            Some(d) if d > 0 => {
                                let transpositions = opts
                                    .get("fuzzy_transpositions")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(true);
                                Box::new(
                                    crate::query::ScoredFuzzy::new(
                                        walked.remove(0),
                                        &way[0],
                                        d,
                                        transpositions,
                                    )
                                    .prefix_length(
                                        opts.get("prefix_length")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0)
                                            as usize,
                                    )
                                    .max_expansions(
                                        opts.get("max_expansions")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(50)
                                            as usize,
                                    ),
                                ) as Box<dyn Query>
                            }
                            _ => Box::new(TermQuery::new(
                                walked.remove(0),
                                IndexRecordOption::WithFreqs,
                            )) as Box<dyn Query>,
                        },
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
        // the best field counts whole and each other field by `tie_breaker`,
        // which was read as an allowed key and then never used
        let tie = body.get("tie_breaker").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        Ok(Box::new(boostcore::query::DisjunctionMaxQuery::with_tie_breaker(subs, tie)))
    }
}

/// `match_bool_prefix`: every analysed term is a term query except the last,
/// which matches as a prefix.
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

/// A span clause of any kind, as the query that walks its matches.
pub(crate) fn build_span(ctx: &Ctx, clause: &Value) -> Result<Box<dyn Query>> {
    let (tree, field) = span_tree(ctx, clause)?;
    let query = crate::query::SpanQuery::new(tree);
    // a keyword keeps no lengths, and a word there is scored as if every
    // value were one word long
    let exact = field.as_deref().map(|f| ctx.resolve(f, true).2 == View::Raw).unwrap_or(false);
    Ok(Box::new(if exact { query.without_norms() } else { query }))
}

/// A nested span clause may not carry a boost of its own: only the query as
/// a whole is scored, so a boost inside it would mean nothing.
fn no_nested_boost(parent: &str, part: &str, clause: &Value) -> Result<()> {
    let boost = clause
        .as_object()
        .and_then(|o| o.values().next())
        .and_then(|body| {
            body.get("boost").or_else(|| {
                body.as_object()
                    .filter(|o| o.len() == 1)
                    .and_then(|o| o.values().next())
                    .and_then(|v| v.get("boost"))
            })
        })
        .and_then(|b| b.as_f64());
    match boost {
        Some(b) if b != 1.0 => Err(anyhow!(
            "{parent} [{part}] as a nested span clause can't have non-default boost value [{b:?}]"
        )),
        _ => Ok(()),
    }
}

/// One nested clause, which has to be a span clause.
fn span_part(
    ctx: &Ctx,
    parent: &str,
    part: &str,
    clause: &Value,
) -> Result<(SpanTree, Option<String>)> {
    let kind = clause.as_object().and_then(|o| o.keys().next()).map(|k| k.as_str()).unwrap_or("");
    if !kind.starts_with("span_") && kind != "field_masking_span" {
        return Err(anyhow!("{parent} [{part}] must be of type span query"));
    }
    no_nested_boost(parent, part, clause)?;
    span_tree(ctx, clause)
}

/// Every clause of a compound span names the same field.
fn same_field(field: &mut Option<String>, clause: Option<String>, complaint: &str) -> Result<()> {
    match (field.as_ref(), clause) {
        (Some(a), Some(b)) if *a != b => Err(anyhow!("{complaint}")),
        (None, Some(b)) => {
            *field = Some(b);
            Ok(())
        }
        _ => Ok(()),
    }
}

/// A span clause, as the tree its matches are walked by, with the field it
/// is over.
pub(crate) fn span_tree(ctx: &Ctx, clause: &Value) -> Result<(SpanTree, Option<String>)> {
    let Some((kind, body)) = clause.as_object().and_then(|o| o.iter().next()) else {
        return Err(anyhow!("[span] clause is empty"));
    };
    let int = |key: &str| body.get(key).and_then(|v| v.as_i64());
    match kind.as_str() {
        "span_term" => {
            let (field, value, opts) = field_and_value(body)?;
            let value = match opts.get("term") {
                Some(term) if opts.get("value").is_none() => term.clone(),
                _ => value,
            };
            let value = match (&value, value.get("term")) {
                (Value::Object(_), Some(term)) => term.clone(),
                _ => value,
            };
            let text = match &value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let (f, path, _) = ctx.resolve(&field, true);
            let mut term = Term::from_field_json_path(f, &path, true);
            term.append_type_and_str(&text);
            Ok((SpanTree::Term(term), Some(field)))
        }
        "span_or" => {
            let clauses = body
                .get("clauses")
                .and_then(|c| c.as_array())
                .ok_or_else(|| anyhow!("span_or must include [clauses]"))?;
            let mut field = None;
            let mut trees = Vec::new();
            for clause in clauses {
                let (tree, named) = span_part(ctx, "span_or", "clauses", clause)?;
                same_field(
                    &mut field,
                    named,
                    "failed to create query: Clauses must have same field.",
                )?;
                trees.push(tree);
            }
            Ok((SpanTree::Or(trees), field))
        }
        "span_near" => {
            let clauses = body
                .get("clauses")
                .and_then(|c| c.as_array())
                .filter(|c| !c.is_empty())
                .ok_or_else(|| anyhow!("span_near must include [clauses]"))?;
            let ordered = body.get("in_order").and_then(|v| v.as_bool()).unwrap_or(true);
            let slop = int("slop").unwrap_or(0) as i32;
            let mut field = None;
            let mut trees = Vec::new();
            for clause in clauses {
                if let Some(gap) = clause.get("span_gap") {
                    if !ordered {
                        return Err(anyhow!(
                            "failed to create query: Gaps can only be added to ordered near queries"
                        ));
                    }
                    let (_, width, _) = field_and_value(gap)?;
                    trees.push(SpanTree::Gap(width.as_i64().unwrap_or(0) as i32));
                    continue;
                }
                let (tree, named) = span_part(ctx, "span_near", "clauses", clause)?;
                if let (Some(f), Some(n)) = (&field, &named)
                    && f != n
                {
                    return Err(anyhow!(
                        "failed to create query: Cannot add clause {n} to SpanNearQuery for field {f}"
                    ));
                }
                same_field(
                    &mut field,
                    named,
                    "failed to create query: Clauses must have same field.",
                )?;
                trees.push(tree);
            }
            Ok((SpanTree::Near { clauses: trees, slop, ordered }, field))
        }
        "span_not" => {
            let include = body
                .get("include")
                .ok_or_else(|| anyhow!("span_not must have [include] span query clause"))?;
            let exclude = body
                .get("exclude")
                .ok_or_else(|| anyhow!("span_not must have [exclude] span query clause"))?;
            let dist = int("dist");
            if dist.is_some() && (int("pre").is_some() || int("post").is_some()) {
                return Err(anyhow!("span_not can either use [dist] or [pre] & [post] (or none)"));
            }
            let (include, field) = span_part(ctx, "span_not", "include", include)?;
            let (exclude, other) = span_part(ctx, "span_not", "exclude", exclude)?;
            if let (Some(a), Some(b)) = (&field, &other)
                && a != b
            {
                return Err(anyhow!("failed to create query: Clauses must have same field."));
            }
            let pre = dist.or(int("pre")).unwrap_or(0).max(0) as i32;
            let post = dist.or(int("post")).unwrap_or(0).max(0) as i32;
            Ok((
                SpanTree::Not { include: Box::new(include), exclude: Box::new(exclude), pre, post },
                field,
            ))
        }
        "span_first" => {
            let inner = body
                .get("match")
                .ok_or_else(|| anyhow!("span_first must have [match] span query clause"))?;
            let end = int("end").ok_or_else(|| anyhow!("span_first must have [end] set for it"))?;
            if end < 0 {
                return Err(anyhow!("parameter [end] needs to be positive."));
            }
            let (inner, field) = span_part(ctx, "span_first", "match", inner)?;
            Ok((SpanTree::First { inner: Box::new(inner), end: end as i32 }, field))
        }
        "span_containing" | "span_within" => {
            let big = body.get("big").ok_or_else(|| anyhow!("{kind} must include [big]"))?;
            let little =
                body.get("little").ok_or_else(|| anyhow!("{kind} must include [little]"))?;
            let (big, field) = span_part(ctx, kind, "big", big)?;
            let (little, other) = span_part(ctx, kind, "little", little)?;
            if let (Some(a), Some(b)) = (&field, &other)
                && a != b
            {
                return Err(anyhow!("failed to create query: big and little not same field"));
            }
            let (big, little) = (Box::new(big), Box::new(little));
            Ok(match kind.as_str() {
                "span_containing" => (SpanTree::Containing { big, little }, field),
                _ => (SpanTree::Within { big, little }, field),
            })
        }
        "field_masking_span" | "span_field_masking" => {
            let inner = body
                .get("query")
                .ok_or_else(|| anyhow!("field_masking_span must have [query] span query clause"))?;
            let masked = body
                .get("field")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("field_masking_span must have [field] set for it"))?;
            let (inner, _) = span_part(ctx, "field_masking_span", "query", inner)?;
            let (f, path, _) = ctx.resolve(masked, true);
            let probe = crate::query::positions::probe_of(f, &path);
            Ok((
                SpanTree::Masked { inner: Box::new(inner), field: probe },
                Some(masked.to_string()),
            ))
        }
        "span_multi" => {
            let inner = body
                .get("match")
                .ok_or_else(|| anyhow!("span_multi must have [match] multi term query clause"))?;
            let (field, terms) = multi_term_words(ctx, inner)?;
            Ok((SpanTree::Or(terms.into_iter().map(SpanTree::Term).collect()), Some(field)))
        }
        "span_gap" => Err(anyhow!("[span_gap] can only be used as a clause of [span_near]")),
        other => Err(anyhow!("unknown span query [{other}]")),
    }
}

/// The indexed words a multi-term query stands for: what a `span_multi`
/// rewrites into before its matches are walked.
pub(crate) fn multi_term_words(ctx: &Ctx, inner: &Value) -> Result<(String, Vec<Term>)> {
    // Lucene refuses a rewrite to more clauses than this rather than walking
    // an unbounded number of words
    const MOST: usize = 1024;
    let Some((kind, body)) = inner.as_object().and_then(|o| o.iter().next()) else {
        return Err(anyhow!("[span_multi] [match] must be of type multi term query"));
    };
    if !matches!(kind.as_str(), "prefix" | "wildcard" | "regexp" | "fuzzy" | "range") {
        return Err(anyhow!("[span_multi] [match] must be of type multi term query"));
    }
    let (field, value, opts) = field_and_value(body)?;
    let (f, path, _) = ctx.resolve(&field, true);
    let text = match &value {
        Value::String(s) => s.clone(),
        Value::Object(_) => String::new(),
        other => other.to_string(),
    };
    let insensitive = is_true(opts.get("case_insensitive"));
    let words = match kind.as_str() {
        "prefix" => dictionary_words(ctx, f, &path, &text, &|_| true, MOST)?,
        "wildcard" => {
            let head = if insensitive { "(?i)" } else { "" };
            let re = regex::Regex::new(&format!("{head}^{}$", wildcard_to_regex(&text)))
                .map_err(|e| anyhow!("bad wildcard `{text}`: {e}"))?;
            dictionary_words(ctx, f, &path, "", &|w| re.is_match(w), MOST)?
        }
        "regexp" => {
            let head = if insensitive { "(?i)" } else { "" };
            let re = regex::Regex::new(&format!("{head}^(?:{text})$"))
                .map_err(|e| anyhow!("bad regex `{text}`: {e}"))?;
            dictionary_words(ctx, f, &path, "", &|w| re.is_match(w), MOST)?
        }
        "range" => {
            let spec = body.get(&field).cloned().unwrap_or(Value::Null);
            let bound = |key: &str| spec.get(key).and_then(|v| v.as_str()).map(|s| s.to_string());
            let (gte, gt, lte, lt) = (bound("gte"), bound("gt"), bound("lte"), bound("lt"));
            dictionary_words(
                ctx,
                f,
                &path,
                "",
                &|w| {
                    gte.as_deref().is_none_or(|b| w >= b)
                        && gt.as_deref().is_none_or(|b| w > b)
                        && lte.as_deref().is_none_or(|b| w <= b)
                        && lt.as_deref().is_none_or(|b| w < b)
                },
                MOST,
            )?
        }
        _ => {
            let mut term = Term::from_field_json_path(f, &path, true);
            term.append_type_and_str(&text);
            let auto = Value::String("AUTO".into());
            let edits = fuzzy_edits(Some(opts.get("fuzziness").unwrap_or(&auto)), &text)
                .unwrap_or(0)
                .min(2);
            let transpositions =
                opts.get("transpositions").and_then(|v| v.as_bool()).unwrap_or(true);
            let searcher = ctx.index.reader()?.searcher();
            crate::query::ScoredFuzzy::new(term, &text, edits, transpositions)
                .prefix_length(
                    opts.get("prefix_length").and_then(|v| v.as_u64()).unwrap_or(0) as usize
                )
                .max_expansions(
                    opts.get("max_expansions").and_then(|v| v.as_u64()).unwrap_or(50) as usize
                )
                .words(&searcher)?
        }
    };
    Ok((field, words))
}

/// The words of a field's dictionary under a path that begin with `prefix`
/// and that `accept` keeps, each once however many segments hold it.
pub(crate) fn dictionary_words(
    ctx: &Ctx,
    field: Field,
    path: &str,
    prefix: &str,
    accept: &dyn Fn(&str) -> bool,
    most: usize,
) -> Result<Vec<Term>> {
    let mut head = Term::from_field_json_path(field, path, true);
    head.append_type_and_str("");
    let head_len = head.serialized_value_bytes().len();
    let mut low = head.serialized_value_bytes().to_vec();
    low.extend_from_slice(prefix.as_bytes());
    let mut high = low.clone();
    let high = loop {
        match high.pop() {
            Some(b) if b < u8::MAX => {
                high.push(b + 1);
                break Some(high);
            }
            Some(_) => continue,
            None => break None,
        }
    };
    let searcher = ctx.index.reader()?.searcher();
    let mut found: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
    for reader in searcher.segment_readers() {
        let inverted = reader.inverted_index(field)?;
        let mut range = inverted.terms().range().ge(&low);
        if let Some(high) = &high {
            range = range.lt(high);
        }
        let mut stream = range.into_stream()?;
        while stream.advance() {
            let key = stream.key();
            let Ok(word) = std::str::from_utf8(&key[head_len..]) else { continue };
            if accept(word) {
                found.insert(key.to_vec());
                if found.len() > most {
                    return Err(anyhow!("maxClauseCount is set to {most}"));
                }
            }
        }
    }
    // built on the path's own term, so each word is still a word under that
    // path -- one rebuilt from its bytes alone would be read as a plain
    // byte term and scored against the whole field's length
    Ok(found
        .into_iter()
        .map(|bytes| {
            let mut term = head.clone();
            term.append_bytes(&bytes[head_len..]);
            term
        })
        .collect())
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
                // every segment holds its own dictionary, and a word two of
                // them hold is still one word
                let found = Term::from_field_bytes(field, bytes);
                if !out.contains(&found) {
                    out.push(found);
                }
                if out.len() >= MOST {
                    return Ok(out);
                }
            }
        }
    }
    Ok(out)
}

/// `common` -- the words split by how many documents hold them.
///
/// A word held by more documents than `cutoff_frequency` names is a common
/// word; the rest are the rare ones. The rare ones are joined by
/// `low_freq_operator` and the common ones by `high_freq_operator`, and where
/// the rare ones are wanted together the common ones may only add to the
/// score of what the rare ones found.
pub(crate) fn build_common(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
    let (field, val, opts) = field_and_value(body)?;
    let text = match &val {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let (f, path, view) = ctx.resolve(&field, true);
    // the words that share a place -- a word and its synonyms -- are one
    // clause between them: any of them is that word
    let edges = analyze_graph(ctx, view, &field, &text, None);
    if edges.is_empty() {
        return Ok(Box::new(EmptyQuery));
    }
    // A word and its synonyms are stacked in one place, and each of them is a
    // clause of its own here rather than the place being one: `minimum_should_match`
    // over `the fast lazy fox brown` counts six, not five, because `quick`
    // stands beside `fast` and a document holding it holds one of the six.
    // That is what makes the difference between `high_freq: 5` and
    // `high_freq: 6` on the same query mean anything.
    let mut places: Vec<(usize, Vec<String>)> = Vec::new();
    for e in &edges {
        places.push((e.from, vec![e.text.clone()]));
    }
    places.sort_by_key(|(p, _)| *p);
    let searcher = ctx.index.reader()?.searcher();
    let cutoff = opts.get("cutoff_frequency").and_then(|v| v.as_f64()).unwrap_or(0.01);
    let most = match cutoff < 1.0 {
        true => (cutoff * searcher.num_docs() as f64).ceil() as u64,
        false => cutoff as u64,
    };
    let operator = |key: &str| -> Occur {
        match opts.get(key).and_then(|v| v.as_str()).map(|s| s.to_ascii_lowercase()) {
            Some(op) if op == "and" => Occur::Must,
            _ => Occur::Should,
        }
    };
    let term_of = |t: &str| -> Term {
        let mut term = Term::from_field_json_path(f, &path, true);
        term.append_type_and_str(t);
        term
    };
    // a place is common where its most frequent word is
    let (mut rare, mut common): (Vec<Vec<Term>>, Vec<Vec<Term>>) = (Vec::new(), Vec::new());
    for (_, words) in &places {
        let terms: Vec<Term> = words.iter().map(|w| term_of(w)).collect();
        let held = terms.iter().map(|t| searcher.doc_freq(t).unwrap_or(0)).max().unwrap_or(0);
        match held >= most {
            true => common.push(terms),
            false => rare.push(terms),
        }
    }
    // how many of the should clauses have to hold, for the rare words and
    // for the common ones
    let msm_of = |key: &str, n: usize| -> usize {
        let spec = opts.get("minimum_should_match");
        let text = match spec {
            Some(Value::Object(o)) => o.get(key).map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            }),
            Some(Value::String(s)) if key == "low_freq" => Some(s.clone()),
            Some(Value::Number(v)) if key == "low_freq" => Some(v.to_string()),
            _ => None,
        };
        match text {
            Some(t) => crate::query::msm_required(Some(&Value::String(t)), n).unwrap_or(0),
            None => 0,
        }
    };
    let clauses_of = |groups: Vec<Vec<Term>>, occur: Occur, key: &str| -> Box<dyn Query> {
        let n = groups.len();
        let min = if occur == Occur::Should { msm_of(key, n) } else { 0 };
        let clauses: Vec<(Occur, Box<dyn Query>)> = groups
            .into_iter()
            .map(|terms| {
                let one: Box<dyn Query> = if terms.len() == 1 {
                    Box::new(TermQuery::new(
                        terms.into_iter().next().unwrap(),
                        IndexRecordOption::WithFreqs,
                    ))
                } else {
                    Box::new(BooleanQuery::union(
                        terms
                            .into_iter()
                            .map(|t| {
                                Box::new(TermQuery::new(t, IndexRecordOption::WithFreqs))
                                    as Box<dyn Query>
                            })
                            .collect(),
                    ))
                };
                (occur, one)
            })
            .collect();
        if min > 0 {
            Box::new(BooleanQuery::with_minimum_required_clauses(clauses, min))
        } else {
            Box::new(BooleanQuery::new(clauses))
        }
    };
    let low = operator("low_freq_operator");
    let high = operator("high_freq_operator");
    Ok(match (rare.is_empty(), common.is_empty()) {
        // With no rare words at all, the common ones are the whole query --
        // and a query of nothing but common words asked for with `should` and
        // no minimum would walk most of the index to rank documents that are
        // all much the same. So every one of them is wanted instead. That is
        // what Lucene's own common-terms query does, and it is why
        // `the fast huge fox` finds the one document holding all four rather
        // than the two holding most of them.
        (true, _) => {
            let min = msm_of("high_freq", common.len());
            let occur = match (min, high) {
                (0, Occur::Should) => Occur::Must,
                _ => high,
            };
            clauses_of(common, occur, "high_freq")
        }
        (_, true) => clauses_of(rare, low, "low_freq"),
        _ => {
            // the rare words are what is asked for; where they are wanted
            // together, the common ones only add to the score
            let rare_query = clauses_of(rare, low, "low_freq");
            let common_query = clauses_of(common, high, "high_freq");
            let want = match low {
                Occur::Must => Occur::Must,
                _ => Occur::Should,
            };
            Box::new(BooleanQuery::new(vec![(want, rare_query), (Occur::Should, common_query)]))
        }
    })
}

/// How many edits `fuzziness` allows for one word: a number as written,
/// or `AUTO` -- none below three letters, one below six, two from there --
/// with `AUTO:lo,hi` moving the two thresholds.
pub(crate) fn fuzzy_edits(spec: Option<&Value>, word: &str) -> Option<u8> {
    let spec = spec?;
    let len = word.chars().count();
    let auto = |lo: usize, hi: usize| {
        if len < lo {
            0
        } else if len < hi {
            1
        } else {
            2
        }
    };
    Some(match spec {
        Value::Number(n) => n.as_u64()?.min(2) as u8,
        Value::String(s) => {
            let s = s.trim();
            if let Some(rest) = s.strip_prefix("AUTO") {
                match rest.strip_prefix(':').and_then(|r| r.split_once(',')) {
                    Some((lo, hi)) => auto(lo.trim().parse().ok()?, hi.trim().parse().ok()?),
                    None => auto(3, 6),
                }
            } else {
                s.parse::<f64>().ok()?.min(2.0) as u8
            }
        }
        _ => return None,
    })
}
