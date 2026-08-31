//! One query name to one BoostCore query: the whole of the query DSL, in
//! the order OpenSearch documents it.

use super::*;

pub fn build(ctx: &Ctx, q: &Value) -> Result<Box<dyn Query>> {
    if let Some(o) = q.as_object()
        && o.len() > 1
    {
        let extra: Vec<&str> = o.keys().skip(1).map(|s| s.as_str()).collect();
        return Err(anyhow!(
            "[query] malformed query, expected [END_OBJECT] but found [{}]",
            extra.join(", ")
        ));
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
            if is_true(opts.get("case_insensitive"))
                && let Some(s) = val.as_str()
            {
                return regex_query(f, &path, &case_insensitive_regex(&escape_regex(s)));
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
            if under_flat && let Some(text) = val.as_str() {
                let mut terms = term_for(f, &path, &val);
                let normal = normalized(ctx, &field, text);
                if normal != text {
                    terms.extend(term_for(f, &path, &Value::String(normal)));
                }
                if let Some(iso) = crate::store::canonical_date(&Value::String(text.to_string()))
                    && iso != text
                {
                    terms.extend(term_for(f, &path, &Value::String(iso)));
                }
                // a number gathered under a flat_object is still a number
                if let Ok(n) = text.parse::<f64>()
                    && let Some(num) = serde_json::Number::from_f64(n)
                {
                    terms.extend(term_for(f, &path, &Value::Number(num)));
                }
                // the values are text like any other, and score like it
                return Ok(any_of(terms));
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
            if view == View::Raw { Box::new(ConstScore::new(exact, 1.0)) } else { exact }
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
            if let Some(n) = vals.as_array().map(|a| a.len())
                && n > ctx.max_terms_count
            {
                return Err(anyhow!(
                    "The number of terms [{n}] used in the Terms Query request has exceeded \
                         the allowed maximum of [{}].",
                    ctx.max_terms_count
                ));
            }
            let (f, path, _) = ctx.resolve(&field, false);
            let arr = vals.as_array().cloned().unwrap_or_default();
            let mut terms = Vec::new();
            let mut subs: Vec<Box<dyn Query>> = Vec::new();
            for v in &arr {
                // a CIDR entry names a range, not a term, so it cannot join the
                // flat term set the common case builds
                match v
                    .as_str()
                    .filter(|s| s.contains('/'))
                    .and(ip_term_query(ctx, &field, f, &path, v))
                {
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
                Box::new(BooleanQuery::new(subs.into_iter().map(|q| (Occur::Should, q)).collect()))
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
                    o.keys().map(|k| k.to_string()).find(|k| {
                        !matches!(
                            k.as_str(),
                            "boost"
                                | "_name"
                                | "ignore_unmapped"
                                | "validation_method"
                                | "type"
                                | "distance"
                                | "distance_type"
                                | "relation"
                        )
                    })
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
            let inner =
                body.get("query").ok_or_else(|| anyhow!("[nested] requires 'query' field"))?;
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
            let terms: Vec<Value> =
                spec.get("terms").and_then(|t| t.as_array()).cloned().unwrap_or_default();
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
            Box::new(BoostQuery::new(Box::new(ConstScore::new(build(ctx, f)?, 1.0)), boost))
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
                "bool",
                "term",
                "terms",
                "match",
                "match_all",
                "match_none",
                "range",
                "prefix",
                "wildcard",
                "regexp",
                "fuzzy",
                "exists",
                "ids",
                "nested",
                "match_phrase",
                "multi_match",
                "query_string",
                "simple_query_string",
                "constant_score",
                "dis_max",
                "boosting",
                "function_score",
                "more_like_this",
            ];
            let near = KNOWN.iter().find(|k| {
                k.len().abs_diff(other.len()) <= 2
                    && k.chars().zip(other.chars()).filter(|(a, b)| a == b).count() + 2 >= k.len()
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
            "prefix" => Some(
                " For optimised prefix queries on text fields please enable \
                              [index_prefixes].",
            ),
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
