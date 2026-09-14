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
    // what follows a query's name is that query's options, and the complaint
    // when it is not says so before the name is looked up at all -- a
    // `{"garbage": "not a query"}` is malformed, not an unknown query, and
    // telling the caller the name is unknown sends them looking in the wrong
    // place
    if !body.is_object() && !matches!(kind.as_str(), "ids" | "match_all" | "match_none") {
        return Err(anyhow!("[{kind}] query malformed, no start_object after query name"));
    }
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
        // the documents some shards hold, for a search narrowed to them
        "_bs_on_shards" => Box::new(OnShards::from_json(&body)?),
        "script" => {
            let Some(spec) = body.get("script") else {
                return Err(anyhow!(
                    "[script] query does not support [{}]",
                    body.as_object()
                        .and_then(|o| o.keys().next())
                        .map(|s| s.as_str())
                        .unwrap_or("")
                ));
            };
            Box::new(ScriptQuery {
                spec: spec.clone(),
                mapping: ctx.mapping.clone(),
                fields: *ctx.fields,
            })
        }
        // the script's score is settled once the candidates are known; here
        // the inner query says which documents there are
        "script_score" => {
            let Some(inner) = body.get("query") else {
                return Err(anyhow!("[script_score] query is required"));
            };
            body.get("script").ok_or_else(|| anyhow!("[script_score] script is required"))?;
            build(ctx, inner)?
        }
        "term" => {
            let (field, val, opts) = field_and_value(&body)?;
            // a join field is asked after by the relation's name, which is
            // kept beside the parent's id under the field
            if ctx.mapping.type_of(&field) == Some("join") {
                let spec = body.get(&field).cloned().unwrap_or(Value::Null);
                return build(ctx, &serde_json::json!({"term": {format!("{field}.name"): spec}}));
            }
            // `_id` is a field of its own, not part of either JSON view, so a
            // term naming it has to be built against that field directly
            // the routing a document was written with is kept in the
            // untouched view under a key no source field may use
            if field == "_routing" {
                let text = match &val {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                return Ok(Box::new(ConstScore::new(
                    any_of(term_for(
                        ctx.fields.raw,
                        crate::store::ROUTING_KEY,
                        &serde_json::json!(text),
                    )),
                    1.0,
                )));
            }
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
            let mut terms = term_for(f, &path, &val);
            // a number or a flag written as text names the value itself, which
            // is how it was written into the index
            if let Some(text) = val.as_str() {
                let read = text
                    .parse::<i64>()
                    .ok()
                    .map(|n| serde_json::json!(n))
                    .or_else(|| text.parse::<f64>().ok().map(|n| serde_json::json!(n)))
                    .or_else(|| text.parse::<bool>().ok().map(|b| serde_json::json!(b)));
                if let Some(read) = read {
                    terms.extend(term_for(f, &path, &read));
                }
            }
            // an exact match on a field that is not analysed has nothing to
            // rank by: every match is equally exact, so each scores one
            if view == View::Raw {
                return Ok(Box::new(ConstScore::new(any_of(terms), 1.0)));
            }
            // on an analysed field a term scores as the word does in `match`,
            // how often it stands there included: read without frequencies,
            // a word twice in a field scored as if it were there once
            if terms.len() == 1 {
                Box::new(TermQuery::new(terms.remove(0), IndexRecordOption::WithFreqs))
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
            }
        }
        "terms" => {
            let (field, vals) = single_key(&body)?;
            if ctx.mapping.type_of(&field) == Some("join") {
                let mut spec = body.clone();
                if let Some(o) = spec.as_object_mut() {
                    o.remove(&field);
                    o.insert(format!("{field}.name"), vals.clone());
                }
                return build(ctx, &serde_json::json!({ "terms": spec }));
            }
            if field == "_routing" {
                let items: Vec<Value> = match &vals {
                    Value::Array(a) => a.clone(),
                    other => vec![other.clone()],
                };
                let terms: Vec<Term> = items
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => Value::String(s.clone()),
                        other => Value::String(other.to_string()),
                    })
                    .flat_map(|v| term_for(ctx.fields.raw, crate::store::ROUTING_KEY, &v))
                    .collect();
                return Ok(Box::new(ConstScore::new(any_of(terms), 1.0)));
            }
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
        // `{"knn": {"embedding": {"vector": [...], "k": 5}}}` -- the documents
        // nearest a point, rather than the ones holding a word.
        //
        // The search happens outside the inverted index, over the table of
        // vectors kept beside it, and comes back as a set of ids with the
        // score each earned. Those become the query: one clause per document,
        // each scoring what its distance was worth. It is the only way to say
        // "these documents, with these scores" in terms an index understands.
        "knn" => {
            let (field, spec) = single_key(&body)?;
            let asked = spec
                .get("vector")
                .and_then(crate::knn::as_vector)
                .ok_or_else(|| anyhow!("[knn] requires a [vector]"))?;
            let declared = ctx
                .mapping
                .vector_fields
                .get(&field)
                .ok_or_else(|| anyhow!("field [{field}] is not knn_vector type"))?;
            if asked.len() != declared.dimension {
                return Err(anyhow!(
                    "Query vector has invalid dimension: {}. Dimension should be: {}",
                    asked.len(),
                    declared.dimension
                ));
            }
            // a filter narrows what may be returned before the distances are
            // compared, so that asking for five near documents among those
            // that also match something else gives five, not five minus
            // however many the filter threw away
            let allowed: Option<std::collections::HashSet<String>> = match spec.get("filter") {
                Some(filter) => Some(ids_matching(ctx, filter)?),
                None => None,
            };
            let keep = allowed.as_ref().map(|set| move |id: &str| set.contains(id));
            let keep_ref: Option<&dyn Fn(&str) -> bool> =
                keep.as_ref().map(|f| f as &dyn Fn(&str) -> bool);
            let held = ctx.vectors.read();
            let space = declared.space;
            // `k` asks for the nearest few; `min_score` and `max_distance`
            // ask for everything close enough, however many that is
            let found = match (
                spec.get("min_score").and_then(|v| v.as_f64()),
                spec.get("max_distance").and_then(|v| v.as_f64()),
            ) {
                (Some(score), _) => {
                    held.within(&field, space, &asked, space.distance_of(score as f32), keep_ref)
                }
                (_, Some(distance)) => {
                    held.within(&field, space, &asked, distance as f32, keep_ref)
                }
                _ => {
                    let k = spec.get("k").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
                    held.nearest(&field, space, &asked, k, keep_ref)
                }
            };
            if found.is_empty() {
                return Ok(Box::new(EmptyQuery));
            }
            let clauses: Vec<(Occur, Box<dyn Query>)> = found
                .into_iter()
                .map(|near| {
                    let term = Term::from_field_text(ctx.fields.id, &near.id);
                    let one: Box<dyn Query> =
                        Box::new(TermQuery::new(term, IndexRecordOption::Basic));
                    (Occur::Should, Box::new(ConstScore::new(one, near.score)) as Box<dyn Query>)
                })
                .collect();
            Box::new(BooleanQuery::new(clauses))
        }
        "exists" => {
            let field = body.get("field").and_then(|f| f.as_str()).unwrap_or_default();
            // A field inside a `nested` object belongs to the hidden documents
            // the nested objects are, not to the document that holds them: at
            // the top of a query it exists in none of them, and only a
            // `nested` query asks after it. It was answered for the parent,
            // so `exists: authors.name` matched every document with an author.
            let parts: Vec<&str> = field.split('.').collect();
            let under_nested = (1..parts.len())
                .any(|n| ctx.mapping.type_of(&parts[..n].join(".")) == Some("nested"));
            // only at the top of a query: inside a `nested` query the same
            // field is the nested object's own, and OpenSearch's own test of
            // `exists` there failed when this answered nothing for it too
            let at_top = NESTED_DEPTH.with(|d| d.get()) == 0;
            if under_nested && at_top {
                return Ok(Box::new(EmptyQuery));
            }
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
            // only a document written with a routing has one
            if field == "_routing" {
                return regex_query(ctx.fields.raw, crate::store::ROUTING_KEY, ".*");
            }
            ctx.exists_query(field)?
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
            ctx.exists_query(field)?
        }
        "prefix" => {
            let (field, val, opts) = field_and_value(&body)?;
            // a field searched while it is typed keeps every beginning of
            // itself, so a prefix of several words is a term rather than a
            // pattern
            let root = field
                .strip_suffix("._2gram")
                .or_else(|| field.strip_suffix("._3gram"))
                .or_else(|| field.strip_suffix("._4gram"))
                .unwrap_or(&field);
            if ctx.mapping.type_of(root) == Some("search_as_you_type")
                && let Some(text) = val.as_str()
            {
                let held = format!("{root}._index_prefix");
                let (f, path, _) = ctx.resolve(&held, false);
                let mut term = Term::from_field_json_path(f, &path, true);
                term.append_type_and_str(text);
                return Ok(Box::new(TermQuery::new(term, IndexRecordOption::Basic)));
            }
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
            let want = val.as_str().unwrap_or_default().to_lowercase();
            // AUTO unless a number is written: the reach grows with the word
            let auto = Value::String("AUTO".into());
            let d = crate::query::text::fuzzy_edits(
                Some(opts.get("fuzziness").unwrap_or(&auto)),
                &want,
            )
            .unwrap_or(0)
            .min(2);
            let mut t = Term::from_field_json_path(f, &path, true);
            t.append_type_and_str(&want);
            if d == 0 {
                Box::new(TermQuery::new(t, IndexRecordOption::WithFreqs))
            } else {
                let transpositions =
                    opts.get("transpositions").and_then(|v| v.as_bool()).unwrap_or(true);
                Box::new(
                    crate::query::ScoredFuzzy::new(t, &want, d, transpositions)
                        .prefix_length(
                            opts.get("prefix_length").and_then(|v| v.as_u64()).unwrap_or(0)
                                as usize,
                        )
                        .max_expansions(
                            opts.get("max_expansions").and_then(|v| v.as_u64()).unwrap_or(50)
                                as usize,
                        ),
                )
            }
        }
        "range" => build_range(ctx, &body)?,
        "match_bool_prefix" => build_match_bool_prefix(ctx, &body)?,
        "query_string" | "simple_query_string" => build_query_string(ctx, &body)?,
        "match" | "match_phrase" | "match_phrase_prefix" => build_match(ctx, &kind, &body)?,
        // the functions are applied to the scores after the search; what the
        // query layer answers is the documents the inner query finds
        "function_score" => {
            let inner =
                body.get("query").cloned().unwrap_or_else(|| serde_json::json!({"match_all": {}}));
            super::build(ctx, &inner)?
        }
        // a rank feature scores by what a field holds; which documents answer
        // is simply which of them hold it
        "rank_feature" => {
            let field = body.get("field").and_then(|v| v.as_str()).unwrap_or("");
            // a log curve rises without bound, so it cannot answer for a
            // feature whose larger values are worth less
            let positive = ctx
                .mapping
                .field_option(field, "positive_score_impact")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            if !positive && body.get("log").is_some() {
                return Err(anyhow!(
                    "Cannot use the [log] function with a field that has a negative score impact \
                     as it would trigger negative scores"
                ));
            }
            super::build(ctx, &serde_json::json!({"exists": {"field": field}}))?
        }
        "span_term" | "span_or" | "span_near" | "span_not" | "span_first" | "span_containing"
        | "span_within" | "span_multi" | "field_masking_span" | "span_field_masking" => {
            build_span(ctx, q)?
        }
        "multi_match" => build_multi_match(ctx, &body)?,
        // `combined_fields` treats the fields it names as one field: a term
        // is satisfied by whichever of them holds it, and the operator is
        // about the terms rather than about any one field.
        //
        // This used to be built as a `cross_fields` multi_match, which asks
        // each field for the whole query and then combines the answers. The
        // difference is not only the score: with `operator: and`, a document
        // whose title holds one word and whose body holds the other answers
        // the reference and was dropped here, because no single field held
        // both. Measured against OpenSearch 3.8.0 -- 3.1.0 does not have the
        // query at all, which is why nothing caught this -- the document sets
        // differed as well as their order.
        //
        // So the query is taken apart by term: each term becomes a
        // disjunction over the fields, and the terms are put together by the
        // operator. The documents are then the reference's documents. The
        // score is not: the reference sums the term frequencies across the
        // fields and scores the sum once, over a combined field length, and
        // that needs statistics this engine does not gather per query. What
        // is here scores each term's best field and adds those up, which
        // ranks nearer than `cross_fields` did and is still an approximation.
        // `docs/progress.md` says so rather than the name implying otherwise.
        "combined_fields" => {
            let fields: Vec<Value> =
                body.get("fields").and_then(|f| f.as_array()).cloned().unwrap_or_default();
            if fields.is_empty() {
                return Err(anyhow!("[combined_fields] requires [fields]"));
            }
            let text = body.get("query").and_then(|v| v.as_str()).unwrap_or_default();
            // every field of a combined_fields query has to share an
            // analyzer for the reference to accept it, so the first names it
            let first = fields
                .first()
                .and_then(|f| f.as_str())
                .map(|f| f.split('^').next().unwrap_or(f).to_string())
                .unwrap_or_default();
            let terms = crate::query::analyze(ctx, crate::query::View::Dyn, &first, text);
            if terms.is_empty() {
                return build(ctx, &serde_json::json!({"match_none": {}}));
            }
            // each field, where its terms are, and what it weighs
            let placed: Vec<(Field, String, f32)> = fields
                .iter()
                .filter_map(|f| f.as_str())
                .map(|spec| {
                    let (name, weight) = match spec.split_once('^') {
                        Some((n, w)) => (n, w.parse::<f32>().unwrap_or(1.0)),
                        None => (spec, 1.0),
                    };
                    let (f, path, _) = ctx.resolve(name, true);
                    (f, path, weight)
                })
                .collect();
            if let Some((_, _, w)) = placed.iter().find(|(_, _, w)| *w < 1.0) {
                return Err(anyhow!("[combined_fields] requires field boosts >= 1.0, got [{w}]"));
            }
            let mut words: Vec<Box<dyn Query>> = Vec::new();
            for t in &terms {
                let per_field: Vec<(Term, f32)> = placed
                    .iter()
                    .map(|(f, path, w)| {
                        let mut term = Term::from_field_json_path(*f, path, true);
                        term.append_type_and_str(t);
                        (term, *w)
                    })
                    .collect();
                words.push(Box::new(crate::query::CombinedTerm::new(per_field)));
            }
            let and = body
                .get("operator")
                .and_then(|v| v.as_str())
                .map(|o| o.eq_ignore_ascii_case("and"))
                == Some(true);
            let occur = if and { Occur::Must } else { Occur::Should };
            let count = words.len();
            let clauses: Vec<(Occur, Box<dyn Query>)> =
                words.into_iter().map(|q| (occur, q)).collect();
            let wanted = if and {
                0
            } else {
                crate::query::bool::msm_required(body.get("minimum_should_match"), count)
                    .unwrap_or(1)
                    .max(1)
            };
            let mut q = BooleanQuery::new(clauses);
            if wanted > 1 {
                q = BooleanQuery::with_minimum_required_clauses(
                    q.clauses().iter().map(|(o, c)| (*o, c.box_clone())).collect(),
                    wanted,
                );
            }
            Box::new(q)
        }
        // Documents here are stored whole rather than split into a parent and
        // its nested children, so a nested query is its inner query asked
        // against the same document -- and `path` is dropped rather than held
        // to.
        //
        // That is a real difference from the reference, not a detail: a
        // `nested` query with two clauses matches a document where one object
        // of the array satisfies the first and a *different* object satisfies
        // the second, where OpenSearch requires one object to satisfy both.
        // Closing it means indexing each nested object as a document of its
        // own and joining the blocks at search time, which is the one thing
        // the storage layer here does not do. It is written down in
        // `docs/progress.md` and in the compatibility notes rather than
        // hidden: a caller relying on nested queries to keep two fields of
        // one object together does not get that here.
        "nested" => {
            let inner =
                body.get("query").ok_or_else(|| anyhow!("[nested] requires 'query' field"))?;
            // inside a `nested` query the nested objects' fields are the
            // documents' own, and `exists` on one of them is a real question
            NESTED_DEPTH.with(|d| d.set(d.get() + 1));
            let built = build(ctx, inner);
            NESTED_DEPTH.with(|d| d.set(d.get() - 1));
            built?
        }
        "intervals" => build_intervals(ctx, &body)?,
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
            if let Some(count_field) =
                spec.get("minimum_should_match_field").and_then(|v| v.as_str())
            {
                // How many terms a document needs is written in the document.
                // The count cannot be read by a scorer, but it takes one of
                // only as many values as there are terms: a document whose
                // count is k needs k of them, which is one clause per k. The
                // floor of one used to stand for every count, so a document
                // asking for three matched on one.
                let mut per_count: Vec<Value> = (1..=terms.len())
                    .map(|k| {
                        serde_json::json!({"bool": {
                            "filter": [{"term": {count_field: k}}],
                            "must": [{"bool": {"should": clauses, "minimum_should_match": k}}],
                        }})
                    })
                    .collect();
                // a count of none or less asks for any one of the terms
                per_count.push(serde_json::json!({"bool": {
                    "filter": [{"range": {count_field: {"lte": 0}}}],
                    "must": [{"bool": {"should": clauses, "minimum_should_match": 1}}],
                }}));
                inner =
                    serde_json::json!({"bool": {"should": per_count, "minimum_should_match": 1}});
            } else if spec.get("minimum_should_match_script").is_some() {
                // a script's count is not read here; one is the floor
                inner["bool"]["minimum_should_match"] = serde_json::json!(1);
            } else if let Some(n) = spec.get("minimum_should_match") {
                inner["bool"]["minimum_should_match"] = n.clone();
            }
            build(ctx, &inner)?
        }
        "bool" => build_bool(ctx, &body)?,
        // A hybrid query's parts are scored apart and put together by a
        // search pipeline, which the search endpoint does before it gets
        // here. Anywhere else -- a count, a bool that only filters it -- it
        // matches what any part matches and scores the sum of the parts.
        "hybrid" => {
            let plain = crate::search::hybrid::as_bool(&body).map_err(|e| anyhow!(e))?;
            return build(ctx, &plain);
        }
        // `common` sorts the words by how many documents hold them: the rare
        // ones are what the query is about, and the common ones only help
        // rank what the rare ones found
        "common" => build_common(ctx, &body)?,
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
            // the best clause counts whole and the others by `tie_breaker`;
            // it was read nowhere, so only the best clause ever counted
            let tie = body.get("tie_breaker").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            Box::new(boostcore::query::DisjunctionMaxQuery::with_tie_breaker(subs?, tie))
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
            "has_child" | "has_parent" | "parent_id" => Some(""),
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
                Some("text") | Some("keyword") | Some("match_only_text") | Some("annotated_text")
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
        .filter(|_| kind != "match_all" && kind != "constant_score" && kind != "script_score");
    Ok(match boost {
        Some(b) => Box::new(BoostQuery::new(inner, b as f32)),
        None => inner,
    })
}

/// Whether a name stands for no query this engine knows.
///
/// Used where a query cannot be built to be told apart -- a template that ran
/// out mid-way still names its clause, and naming one that does not exist is
/// a complaint about the name rather than about the text.
pub(crate) fn unknown_clause(name: &str) -> bool {
    const CLAUSES: &[&str] = &[
        "_bs_on_shards",
        "bool",
        "boosting",
        "combined_fields",
        "common",
        "constant_score",
        "dis_max",
        "distance_feature",
        "exists",
        "field_masking_span",
        "function_score",
        "fuzzy",
        "geo_bounding_box",
        "geo_distance",
        "geo_polygon",
        "geo_shape",
        "has_child",
        "hybrid",
        "has_parent",
        "ids",
        "intervals",
        "knn",
        "match",
        "match_all",
        "match_bool_prefix",
        "match_none",
        "match_phrase",
        "match_phrase_prefix",
        "more_like_this",
        "multi_match",
        "nested",
        "parent_id",
        "percolate",
        "prefix",
        "query_string",
        "range",
        "rank_feature",
        "regexp",
        "simple_query_string",
        "span_containing",
        "span_first",
        "span_gap",
        "span_multi",
        "span_near",
        "span_not",
        "span_or",
        "span_term",
        "span_within",
        "term",
        "terms",
        "terms_set",
        "wildcard",
        "wrapper",
    ];
    !CLAUSES.contains(&name)
}

/// The ids of the documents a filter matches.
///
/// A `knn` filter narrows the field before the distances are compared, not
/// after: asking for the five nearest documents that also match something
/// else should give five, not five minus however many the filter removed.
/// That means knowing which documents the filter matches before the search,
/// which is what this is for.
fn ids_matching(ctx: &Ctx, filter: &Value) -> Result<std::collections::HashSet<String>> {
    let query = super::build(ctx, filter)?;
    let reader =
        ctx.index.reader_builder().reload_policy(boostcore::ReloadPolicy::Manual).try_into()?;
    let searcher: boostcore::Searcher = reader.searcher();
    let found = searcher.search(&query, &boostcore::collector::DocSetCollector)?;
    let mut out = std::collections::HashSet::with_capacity(found.len());
    for address in found {
        let Some(reader) = searcher.segment_readers().get(address.segment_ord as usize) else {
            continue;
        };
        let Ok(Some(column)) = reader.fast_fields().str("_id") else { continue };
        let Some(ord) = column.term_ords(address.doc_id).next() else { continue };
        let mut id = String::new();
        if column.ord_to_str(ord, &mut id).is_ok() {
            out.insert(id);
        }
    }
    Ok(out)
}

thread_local! {
    /// How many `nested` queries the query being built sits inside.
    static NESTED_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}
