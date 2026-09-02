//! One search, from the request to the answer.

use super::*;

/// Every `rank_feature` clause a query holds, wherever it stands in it.
fn collect_rank_features(node: &Value, out: &mut Vec<Value>) {
    match node {
        Value::Object(o) => {
            for (key, value) in o {
                if key == "rank_feature" {
                    out.push(value.clone());
                } else {
                    collect_rank_features(value, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_rank_features(item, out)),
        _ => {}
    }
}

/// The score a rank feature asks for: the value of a field, curved.
///
/// A feature says how much a document is worth on its own -- how many people
/// link to it, how short its address is -- and the curve says how quickly that
/// worth stops mattering.
fn rescore_by_rank_features(
    searchers: &[(String, boostcore::Searcher, std::sync::Arc<parking_lot::RwLock<IdxState>>)],
    cands: &mut [Cand],
    features: &[Value],
) {
    for cand in cands.iter_mut() {
        let (_, searcher, st) = &searchers[cand.shard];
        let g = st.read();
        let Some((_, source)) = source_of(searcher, &g, cand.addr) else { continue };
        let mut total = 0.0f32;
        for spec in features {
            let field = spec.get("field").and_then(|v| v.as_str()).unwrap_or("");
            let held = source
                .pointer(&format!("/{}", field.replace('.', "/")))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as f32;
            let boost = spec.get("boost").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
            // whether a larger value is worth more is the field's own
            // property, which the query may not override
            let positive = g
                .mapping
                .field_option(field, "positive_score_impact")
                .and_then(|v| v.as_bool())
                // a `rank_features` field is a map of features, so the option
                // stands on the field the feature is written under
                .or_else(|| {
                    let (parent, _) = field.rsplit_once('.')?;
                    g.mapping.field_option(parent, "positive_score_impact")?.as_bool()
                })
                .or_else(|| spec.get("positive_score_impact").and_then(|v| v.as_bool()))
                .unwrap_or(true);
            // a feature whose larger values are worth less is held as its own
            // reciprocal, so every curve below is written the one way round
            let value = match positive {
                true => held,
                false => 1.0 / held.max(f32::MIN_POSITIVE),
            };
            let curved = if let Some(log) = spec.get("log") {
                let scaling =
                    log.get("scaling_factor").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
                (scaling + value).ln()
            } else if let Some(saturation) = spec.get("saturation") {
                let pivot = saturation
                    .get("pivot")
                    .and_then(|v| v.as_f64())
                    .map(|p| p as f32)
                    .unwrap_or(value.max(1.0));
                value / (value + pivot)
            } else if let Some(sigmoid) = spec.get("sigmoid") {
                let pivot = sigmoid.get("pivot").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
                let exponent =
                    sigmoid.get("exponent").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
                value.powf(exponent) / (value.powf(exponent) + pivot.powf(exponent))
            } else if spec.get("linear").is_some() {
                value
            } else {
                // without a curve named, saturation with the value as its own
                // pivot is what OpenSearch settles on
                value / (value + 1.0)
            };
            total += boost * curved;
        }
        cand.score = total;
    }
}

/// The score `function_score` asks for, in place of the one the query gave.
///
/// A function may name a filter -- it counts only for the documents that
/// match it -- and either a weight, or a field whose value stands for how
/// much the document is worth. `boost_mode` says how what the functions make
/// meets what the query scored.
/// A script's failure, reported the way a search reports one: the shards
/// failed, and the script exception is why.
pub(crate) fn search_script_failure(e: crate::painless::ScriptError, index: &str) -> Response {
    let detail = e.to_json();
    let mut root = detail.clone();
    if let Some(o) = root.as_object_mut() {
        o.remove("caused_by");
    }
    let body = json!({
        "error": {
            "root_cause": [root],
            "type": "search_phase_execution_exception",
            "reason": "all shards failed",
            "phase": "query",
            "grouped": true,
            "failed_shards": [{
                "shard": 0,
                "index": index,
                "node": "node0",
                "reason": detail,
            }],
        },
        "status": 400,
    });
    axum::response::IntoResponse::into_response((StatusCode::BAD_REQUEST, axum::Json(body)))
}

/// A failure of one kind and reason, reported as the shards failing.
pub(crate) fn search_shard_failure(kind: &str, reason: &str, index: &str) -> Response {
    let body = json!({
        "error": {
            "root_cause": [{"type": kind, "reason": reason}],
            "type": "search_phase_execution_exception",
            "reason": "all shards failed",
            "phase": "query",
            "grouped": true,
            "failed_shards": [{
                "shard": 0,
                "index": index,
                "node": "node0",
                "reason": {"type": kind, "reason": reason},
            }],
        },
        "status": 400,
    });
    axum::response::IntoResponse::into_response((StatusCode::BAD_REQUEST, axum::Json(body)))
}

/// The term statistics a score script asks for, read from the segment the
/// document sits in: how often a term appears in this document, in how many
/// documents, and how many tokens the field holds in all.
fn term_stats_for(
    searcher: &boostcore::Searcher,
    st: &IdxState,
    addr: DocAddress,
) -> Box<dyn Fn(&str, &str, &str) -> f64> {
    use boostcore::schema::IndexRecordOption;
    let reader = searcher.segment_reader(addr.segment_ord).clone();
    let fields = st.fields;
    let mapping = st.mapping.clone();
    let doc = addr.doc_id;
    Box::new(move |what: &str, field: &str, term: &str| -> f64 {
        // a keyword is kept whole in the raw view; text is tokenised into
        // the dynamic one, and a term of it is one lowercased token
        let kind = mapping.type_of(field).unwrap_or("keyword");
        let (f, text) = if matches!(kind, "text" | "match_only_text") {
            (fields.dynamic, term.to_lowercase())
        } else {
            (fields.raw, term.to_string())
        };
        let mut t = boostcore::schema::Term::from_field_json_path(f, field, true);
        t.append_type_and_str(&text);
        let Ok(inverted) = reader.inverted_index(f) else { return 0.0 };
        match what {
            "termFreq" => inverted
                .read_postings(&t, IndexRecordOption::WithFreqs)
                .ok()
                .flatten()
                .map(|mut postings| {
                    use boostcore::{DocSet, postings::Postings};
                    if postings.seek(doc) == doc { postings.term_freq() as f64 } else { 0.0 }
                })
                .unwrap_or(0.0),
            "docFreq" => inverted.doc_freq(&t).map(|n| n as f64).unwrap_or(0.0),
            "totalTermFreq" => inverted
                .read_postings(&t, IndexRecordOption::WithFreqs)
                .ok()
                .flatten()
                .map(|mut postings| {
                    use boostcore::{DocSet, postings::Postings};
                    let mut total = 0.0;
                    let mut d = postings.doc();
                    while d != boostcore::TERMINATED {
                        total += postings.term_freq() as f64;
                        d = postings.advance();
                    }
                    total
                })
                .unwrap_or(0.0),
            // the field's terms sit together in the dictionary, under the
            // path they share; each one's postings say how often it appears
            "sumTotalTermFreq" | "sumDocFreq" => {
                let prefix = boostcore::schema::Term::from_field_json_path(f, field, true);
                let low = prefix.serialized_value_bytes().to_vec();
                let mut high = low.clone();
                high.push(0xff);
                let Ok(mut stream) = inverted.terms().range().ge(&low).lt(&high).into_stream()
                else {
                    return 0.0;
                };
                let mut total = 0.0;
                while stream.advance() {
                    let info = stream.value().clone();
                    if what == "sumDocFreq" {
                        total += info.doc_freq as f64;
                        continue;
                    }
                    if let Ok(mut postings) =
                        inverted.read_postings_from_terminfo(&info, IndexRecordOption::WithFreqs)
                    {
                        use boostcore::{DocSet, postings::Postings};
                        let mut d = postings.doc();
                        while d != boostcore::TERMINATED {
                            total += postings.term_freq() as f64;
                            d = postings.advance();
                        }
                    }
                }
                total
            }
            _ => 0.0,
        }
    })
}

/// `script_score`: the score is what the script says, given the query's
/// score and the document.
fn rescore_by_script(
    searchers: &[(String, boostcore::Searcher, std::sync::Arc<parking_lot::RwLock<IdxState>>)],
    cands: &mut Vec<Cand>,
    spec: &Value,
) -> std::result::Result<(), Response> {
    let Some(script) = spec.get("script") else { return Ok(()) };
    let min_score = spec.get("min_score").and_then(|v| v.as_f64());
    let boost = spec.get("boost").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
    let mut failure = None;
    cands.retain_mut(|cand| {
        if failure.is_some() {
            return true;
        }
        let (name, searcher, st) = &searchers[cand.shard];
        let g = st.read();
        let Some((_, source)) = source_of(searcher, &g, cand.addr) else { return true };
        let expanded = crate::store::expand_for_indexing(source, &g.mapping);
        let stats = term_stats_for(searcher, &g, cand.addr);
        match crate::painless::contexts::run_on_doc_with(
            script,
            &expanded,
            &g.mapping,
            cand.score as f64,
            Some(stats),
        ) {
            Ok(v) => {
                let made = v.as_f64().unwrap_or(0.0);
                if made < 0.0 {
                    failure = Some(search_shard_failure(
                        "illegal_argument_exception",
                        &format!(
                            "script score function must not produce negative scores, but got: \
                             [{made}]"
                        ),
                        name,
                    ));
                    return true;
                }
                cand.score = made as f32 * boost;
                min_score.map(|m| made >= m).unwrap_or(true)
            }
            Err(e) => {
                failure = Some(search_script_failure(e, name));
                true
            }
        }
    });
    match failure {
        Some(r) => Err(r),
        None => Ok(()),
    }
}

fn rescore_by_functions(
    searchers: &[(String, boostcore::Searcher, std::sync::Arc<parking_lot::RwLock<IdxState>>)],
    cands: &mut [Cand],
    spec: &Value,
) -> std::result::Result<(), Response> {
    let mut functions: Vec<Value> =
        spec.get("functions").and_then(|f| f.as_array()).cloned().unwrap_or_default();
    // a single function may be written beside the query rather than in a list
    for named in ["field_value_factor", "weight", "random_score", "script_score"] {
        if let Some(one) = spec.get(named) {
            functions.push(json!({ named: one }));
        }
    }
    if functions.is_empty() {
        return Ok(());
    }
    let score_mode = spec.get("score_mode").and_then(|v| v.as_str()).unwrap_or("multiply");
    let boost_mode = spec.get("boost_mode").and_then(|v| v.as_str()).unwrap_or("multiply");
    let query_boost = spec.get("boost").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
    for cand in cands.iter_mut() {
        let (name, searcher, st) = &searchers[cand.shard];
        let g = st.read();
        let Some((_, source)) = source_of(searcher, &g, cand.addr) else { continue };
        let mut made: Vec<f32> = Vec::new();
        // the script sees the document as the index read it
        let mut expanded: Option<Value> = None;
        for function in &functions {
            // a function with a filter counts only where the filter matches
            if let Some(filter) = function.get("filter")
                && !matches_here(&source, filter)
            {
                continue;
            }
            let weight = function.get("weight").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
            let value = match function.get("field_value_factor") {
                None if function.get("script_score").is_some() => {
                    let script = function.pointer("/script_score/script").unwrap_or(&Value::Null);
                    let seen = expanded.get_or_insert_with(|| {
                        crate::store::expand_for_indexing(source.clone(), &g.mapping)
                    });
                    let stats = term_stats_for(searcher, &g, cand.addr);
                    match crate::painless::contexts::run_on_doc_with(
                        script,
                        seen,
                        &g.mapping,
                        cand.score as f64,
                        Some(stats),
                    ) {
                        Ok(v) => {
                            let made = v.as_f64().unwrap_or(0.0);
                            if made < 0.0 {
                                return Err(search_shard_failure(
                                    "illegal_argument_exception",
                                    &format!(
                                        "script score function must not produce negative \
                                         scores, but got: [{made}]"
                                    ),
                                    name,
                                ));
                            }
                            made as f32
                        }
                        Err(e) => return Err(search_script_failure(e, name)),
                    }
                }
                Some(spec) => {
                    let field = spec.get("field").and_then(|v| v.as_str()).unwrap_or("");
                    let factor = spec.get("factor").and_then(|v| v.as_f64()).unwrap_or(1.0);
                    let missing = spec.get("missing").and_then(|v| v.as_f64());
                    let held = source
                        .pointer(&format!("/{}", field.replace('.', "/")))
                        .and_then(|v| v.as_f64())
                        .or(missing)
                        .unwrap_or(0.0);
                    let scaled = held * factor;
                    (match spec.get("modifier").and_then(|v| v.as_str()).unwrap_or("none") {
                        "log" => scaled.log10(),
                        "log1p" => (1.0 + scaled).log10(),
                        "log2p" => (2.0 + scaled).log10(),
                        "ln" => scaled.ln(),
                        "ln1p" => (1.0 + scaled).ln_1p(),
                        "ln2p" => (2.0 + scaled).ln(),
                        "square" => scaled * scaled,
                        "sqrt" => scaled.sqrt(),
                        "reciprocal" => {
                            if scaled == 0.0 {
                                0.0
                            } else {
                                1.0 / scaled
                            }
                        }
                        _ => scaled,
                    }) as f32
                }
                None => 1.0,
            };
            made.push(weight * value);
        }
        if made.is_empty() {
            continue;
        }
        let combined = match score_mode {
            "sum" => made.iter().sum(),
            "avg" => made.iter().sum::<f32>() / made.len() as f32,
            "first" => made[0],
            "max" => made.iter().cloned().fold(f32::MIN, f32::max),
            "min" => made.iter().cloned().fold(f32::MAX, f32::min),
            _ => made.iter().product(),
        };
        cand.score = match boost_mode {
            "replace" => combined,
            "sum" => cand.score + combined,
            "avg" => (cand.score + combined) / 2.0,
            "max" => cand.score.max(combined),
            "min" => cand.score.min(combined),
            _ => cand.score * combined,
        } * query_boost;
    }
    Ok(())
}

/// Whether a document, as it stands, answers a simple filter.
///
/// Only the filters a function names are read here -- a term, a range, a
/// match on one field -- which is what `function_score` puts in front of a
/// weight.
fn matches_here(source: &Value, filter: &Value) -> bool {
    let Some((kind, body)) = filter.as_object().and_then(|o| o.iter().next()) else {
        return true;
    };
    let held = |field: &str| source.pointer(&format!("/{}", field.replace('.', "/"))).cloned();
    match kind.as_str() {
        "match_all" => true,
        "match_none" => false,
        "term" | "match" | "match_phrase" => {
            let Some((field, wanted)) = body.as_object().and_then(|o| o.iter().next()) else {
                return false;
            };
            let wanted = wanted.get("value").or_else(|| wanted.get("query")).unwrap_or(wanted);
            match held(field) {
                Some(Value::String(s)) => wanted.as_str().map(|w| s.contains(w)).unwrap_or(false),
                Some(other) => &other == wanted,
                None => false,
            }
        }
        "terms" => {
            let Some((field, wanted)) = body.as_object().and_then(|o| o.iter().next()) else {
                return false;
            };
            let held = held(field);
            wanted
                .as_array()
                .map(|any| any.iter().any(|w| held.as_ref() == Some(w)))
                .unwrap_or(false)
        }
        "range" => {
            let Some((field, bounds)) = body.as_object().and_then(|o| o.iter().next()) else {
                return false;
            };
            let Some(value) = held(field).and_then(|v| v.as_f64()) else { return false };
            let past = |name: &str, ok: fn(f64, f64) -> bool| {
                bounds
                    .get(name)
                    .and_then(|v| v.as_f64())
                    .map(|edge| ok(value, edge))
                    .unwrap_or(true)
            };
            past("gte", |v, e| v >= e)
                && past("gt", |v, e| v > e)
                && past("lte", |v, e| v <= e)
                && past("lt", |v, e| v < e)
        }
        "exists" => {
            body.get("field").and_then(|v| v.as_str()).map(|f| held(f).is_some()).unwrap_or(false)
        }
        "bool" => {
            let all = |name: &str, want: bool| {
                body.get(name)
                    .and_then(|v| v.as_array())
                    .map(|cs| cs.iter().all(|c| matches_here(source, c) == want))
                    .unwrap_or(true)
            };
            all("must", true) && all("filter", true) && all("must_not", false)
        }
        _ => true,
    }
}

/// The ids a query matches, for the passes that narrow a page rather than
/// build one.
fn matching_ids(
    store: &Store,
    targets: &[String],
    query: &Value,
) -> std::result::Result<std::collections::HashSet<String>, Response> {
    let probe = json!({"query": query, "size": 10_000, "_source": false});
    let found = run(store, &targets.join(","), &probe, &Params::new())?;
    Ok(found
        .hits
        .iter()
        .filter_map(|hit| hit.get("_id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect())
}

/// Run a search across every resolved index and merge the results.
pub fn run(
    store: &Store,
    expr: &str,
    body: &Value,
    p: &Params,
) -> std::result::Result<Outcome, Response> {
    const BODY_KEYS: &[&str] = &[
        "query",
        "from",
        "size",
        "sort",
        "_source",
        "aggs",
        "aggregations",
        "post_filter",
        "highlight",
        "track_total_hits",
        "track_scores",
        "stored_fields",
        "docvalue_fields",
        "script_fields",
        "explain",
        "version",
        "seq_no_primary_term",
        "min_score",
        "timeout",
        "terminate_after",
        "search_after",
        "collapse",
        "rescore",
        "indices_boost",
        "profile",
        "suggest",
        "fields",
        "runtime_mappings",
        "slice",
        "pit",
        "stats",
        "batched_reduce_size",
        "ext",
        "knn",
    ];
    if let Some(o) = body.as_object() {
        for k in o.keys() {
            if !BODY_KEYS.contains(&k.as_str()) {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "parsing_exception",
                    format!("Unknown key for a START_OBJECT in [{k}]."),
                ));
            }
        }
    }
    for key in ["from", "size"] {
        if let Some(n) = as_i64(body_or_param(body, p, key))
            && n < 0
        {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("[{key}] parameter cannot be negative, found [{n}]"),
            ));
        }
    }
    if let Some(n) = as_i64(p.get("batched_reduce_size").map(|v| json!(v)))
        && n < 2
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("batchedReduceSize must be >= 2, got {n}"),
        ));
    }
    if let Some(n) = as_i64(p.get("pre_filter_shard_size").map(|v| json!(v)))
        && n < 1
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("preFilterShardSize must be >= 1, got {n}"),
        ));
    }
    if let Some(n) = as_i64(
        body.get("track_total_hits")
            .cloned()
            .or_else(|| p.get("track_total_hits").map(|v| json!(v))),
    ) && n < -1
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("[track_total_hits] parameter must be positive or equals to -1, got {n}"),
        ));
    }
    if let Some(st) = p.get("search_type")
        && (st == "query_and_fetch" || st == "dfs_query_and_fetch")
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("Unsupported search type [{st}]"),
        ));
    }
    validate_params(body, p)?;
    let from = as_usize(body_or_param(body, p, "from")).unwrap_or(0);
    let size = as_usize(body_or_param(body, p, "size")).unwrap_or(10);
    // reading documents skips the closed indices a pattern would otherwise
    // reach; a closed index named outright is a different complaint
    // `pit` names a point in time rather than an index expression: it carries
    // both which indices to search and how far into each to look
    let pit = body
        .get("pit")
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .and_then(|id| store.read_pit(id));
    let expr: &str = match pit.as_ref() {
        Some(p) if expr.is_empty() => &p.expr,
        _ => expr,
    };
    let pit_ceiling: std::collections::HashMap<String, u64> =
        pit.as_ref().map(|p| p.ceiling.clone()).unwrap_or_default();
    let targets = store.resolve_open(expr);
    check_limits(store, &targets, body, p, from, size)?;
    // `ignore_unavailable` says to pass over what cannot be searched rather
    // than to complain about it
    let lenient = p.get("ignore_unavailable").map(|v| v != "false").unwrap_or(false);
    // `expand_wildcards` naming closed indices means a pattern reaches them,
    // and a closed index cannot be searched whichever way it was reached
    let wants_closed = p
        .get("expand_wildcards")
        .map(|v| v.split(',').any(|w| matches!(w.trim(), "closed" | "all")))
        .unwrap_or(false);
    if wants_closed && !lenient {
        for name in store.resolve(expr) {
            if store.is_closed(&name) {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "index_closed_exception",
                    format!("closed index [{name}]"),
                ));
            }
        }
    }
    for name in
        expr.split(',').map(|n| n.trim()).filter(|n| !n.is_empty() && !n.contains('*') && !lenient)
    {
        if store.is_closed(name) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "index_closed_exception",
                format!("closed index [{name}]"),
            ));
        }
    }
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !expr.is_empty() && !lenient {
        // a date-math name is reported as the index it stands for, since that
        // is the one that was not there
        return Err(no_such_index(&crate::store::resolve_date_math_name(expr)));
    }
    // `allow_no_indices=false` makes an expression that reaches nothing an
    // error rather than a search with nothing to search
    if targets.is_empty()
        && !expr.is_empty()
        && p.get("allow_no_indices").map(|v| v == "false").unwrap_or(false)
    {
        return Err(no_such_index(expr));
    }
    // a `terms` lookup names a document to read the term list from
    // A shard whose documents an aggregation cannot take answers with an
    // error rather than with a result, and the search goes on without it.
    // Here the one that fails is the one holding a value the sketch refuses.
    let mut failures: Vec<Value> = Vec::new();
    let mut excluded_ids: Vec<String> = Vec::new();
    if let Some(field) =
        body.get("aggs").or_else(|| body.get("aggregations")).and_then(hdr_percentiles_field)
    {
        let shards = targets
            .iter()
            .filter_map(|n| store.get(n))
            .map(|st| st.read().shard_count())
            .max()
            .unwrap_or(1);
        let probe = json!({"query": {"range": {field.clone(): {"lt": 0}}}, "size": 1});
        let refused = run(store, &targets.join(","), &probe, &Params::new())
            .ok()
            .and_then(|o| o.hits.first().and_then(|h| h.get("_id")?.as_str().map(String::from)));
        if let Some(id) = refused {
            let bad = routing_shard(&id, shards);
            let all = json!({"query": {"match_all": {}}, "size": 10_000, "_source": false});
            if let Ok(o) = run(store, &targets.join(","), &all, &Params::new()) {
                for hit in &o.hits {
                    let Some(other) = hit.get("_id").and_then(|v| v.as_str()) else { continue };
                    if routing_shard(other, shards) == bad {
                        excluded_ids.push(other.to_string());
                    }
                }
            }
            failures.push(json!({
                "shard": bad,
                "index": targets.first().cloned().unwrap_or_default(),
                "node": "node-0",
                "reason": {
                    "type": "array_index_out_of_bounds_exception",
                    "reason": "-1",
                },
            }));
        }
    }
    let mut extras = Extras::default();
    if let Some(q) = body.get("query") {
        scan_extras(q, &mut extras);
    }
    let extras = extras;
    let mut query_json = body.get("query").cloned();
    if !excluded_ids.is_empty() {
        let base = query_json.take().unwrap_or_else(|| json!({"match_all": {}}));
        query_json = Some(json!({
            "bool": {
                "must": [base],
                "must_not": [{"ids": {"values": excluded_ids.clone()}}],
            }
        }));
    }
    // A document's routing is not part of it -- it is how the document was
    // addressed -- so asking which documents have one is asking after a list
    // of ids rather than after a column.
    if let Some(q) = query_json.as_mut()
        && extras.routing_exists
    {
        let ids: Vec<String> = targets
            .iter()
            .filter_map(|n| store.get(n))
            .flat_map(|st| st.read().routing.keys().cloned().collect::<Vec<_>>())
            .collect();
        replace_routing_exists(q, &ids);
    }
    // what a join asked to list is read before the join is rewritten away
    let mut join_inner_hits: Vec<(String, String, Value, Value)> = Vec::new();
    if let Some(q) = query_json.as_mut() {
        resolve_terms_lookups(store, q)?;
        expand_bitmap_terms(q);
        expand_more_like_this(store, &targets, q);
        // a joining query walks one set of documents to answer about another,
        // which is one of the costs a cluster may have turned off
        if !expensive_allowed(store) && names_a_join(q) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                "[joining] queries cannot be executed when 'search.allow_expensive_queries' is \
                 set to false.",
            ));
        }
        collect_join_inner_hits(q, &mut join_inner_hits);
        expand_joins(store, &targets, q);
        if names_a_percolate(q) {
            expand_percolate(store, &targets, q);
        }
    }

    // a field cannot be both kept and dropped: naming it in both lists asks
    // for two answers about the same field
    if let (Some(inc), Some(exc)) = (
        body.pointer("/_source/includes").and_then(|v| v.as_array()),
        body.pointer("/_source/excludes").and_then(|v| v.as_array()),
    ) && let Some(both) = inc.iter().find(|i| exc.contains(i)).and_then(|v| v.as_str())
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("The same entry [{both}] cannot be both included and excluded in _source."),
        ));
    }

    // `_shard_doc` orders by where a document sits within a shard, which only
    // holds still while a point-in-time is open; without one the order it
    // names does not exist
    if body.get("pit").is_none() {
        let names_shard_doc = |v: &Value| match v {
            Value::String(s) => s == "_shard_doc",
            Value::Object(o) => o.keys().any(|k| k == "_shard_doc"),
            _ => false,
        };
        let asked = match body.get("sort") {
            Some(Value::Array(a)) => a.iter().any(names_shard_doc),
            Some(one) => names_shard_doc(one),
            None => false,
        };
        if asked {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "action_request_validation_exception",
                "Validation Failed: 1: _shard_doc is only supported with point-in-time;",
            ));
        }
    }

    // unsigned_long cannot be sorted alongside another numeric type
    let mut sort_keys = parse_sort(body.get("sort"));
    // `_shard_doc` orders by where a document sits within a shard. That is the
    // order it was written in, which is what `_seq` records -- and it only
    // holds still while a point in time is open, which is why it is refused
    // without one.
    for k in sort_keys.iter_mut() {
        if k.field == "_shard_doc" {
            k.field = "_seq".to_string();
        }
        // a join field is sorted by the relation each document stands in,
        // which is what the field's own value is
        let joined = targets
            .iter()
            .filter_map(|n| store.get(n))
            .any(|st| st.read().mapping.type_of(&k.field) == Some("join"));
        if joined {
            k.field = format!("{}.name", k.field);
        }
    }
    // `_doc` is the order the index holds its documents in, and an index that
    // was told to sort itself holds them in that order
    if sort_keys.len() == 1 && sort_keys[0].field == "_doc" {
        let declared = targets.iter().filter_map(|n| store.get(n)).find_map(|st| {
            let g = st.read();
            let fields = g.setting("sort.field")?;
            let orders = g.setting("sort.order").unwrap_or_default();
            let orders: Vec<String> =
                orders.split(',').map(|s| s.trim().trim_matches('"').to_string()).collect();
            let keys: Vec<SortKey> = fields
                .trim_matches(|c| c == '[' || c == ']')
                .split(',')
                .map(|f| f.trim().trim_matches('"').to_string())
                .filter(|f| !f.is_empty())
                .enumerate()
                .map(|(i, field)| SortKey {
                    field,
                    desc: orders.get(i).map(|o| o == "desc").unwrap_or(false),
                    mode: None,
                    missing_last: true,
                    nested: None,
                    nested_filter: None,
                    numeric_type: None,
                })
                .collect();
            (!keys.is_empty()).then_some(keys)
        });
        if let Some(keys) = declared {
            sort_keys = keys;
        }
    }
    for k in &sort_keys {
        let mut kinds: Vec<String> = Vec::new();
        for n in &targets {
            if let Some(st) = store.get(n)
                && let Some(t) = st.read().mapping.type_of(&k.field)
                && !kinds.contains(&t.to_string())
            {
                kinds.push(t.to_string());
            }
        }
        if kinds.len() > 1 && kinds.iter().any(|t| t == "unsigned_long") {
            return Err(err_caused_by(
                "search_phase_execution_exception",
                "all shards failed",
                "Can't do sort across indices, as a field has [unsigned_long] type in one index, \
                 and different type in another index!",
            ));
        }
    }
    let AggPlan {
        request: agg_json,
        peeled: filters_aggs,
        siblings: pipeline_aggs,
        inner: bucket_pipelines,
        weighted,
    } = plan_aggs(store, &targets, body)?;

    let OutputSpecs { source: source_sel, fields: field_specs, stored } =
        output_specs(store, &targets, body, p)?;

    let started = std::time::Instant::now();
    // a slice divides the index between readers, so the page it can offer is
    // cut from every matching document rather than from the first few
    let slice = body.get("slice").filter(|s| s.get("max").is_some()).cloned();
    // collapsing decides the page from groups rather than from documents, so
    // the best few documents are not enough to cut it from
    // a sort that only counts some of a document's nested objects is settled
    // after the candidates are in hand, so the page cannot be cut while
    // collecting
    let nested_filtered = sort_keys.iter().any(|k| k.nested_filter.is_some());
    let page_want = if slice.is_some() || body.get("collapse").is_some() || nested_filtered {
        65_536
    } else {
        from + size
    };
    let mut cands: Vec<Cand> = Vec::new();
    let mut searchers: Vec<(String, Searcher, std::sync::Arc<parking_lot::RwLock<IdxState>>)> =
        Vec::new();
    let mut total: u64 = 0;
    let mut shards: u64 = 0;
    let mut empty_shards: u64 = 0;
    let agg_acc: Option<IntermediateAggregationResults>;
    let mut agg_req: Option<Aggregations> = None;
    let mut fruits: Vec<IntermediateAggregationResults> = Vec::new();
    let mut shard_profiles: Vec<Value> = Vec::new();
    let mut agg_meta: Vec<(String, Value)> = Vec::new();
    let mut bucket_orders: Vec<(String, String, bool)> = Vec::new();
    // which slice of the term space was asked for is a property of the
    // request, not of any one shard, so it is read once here
    let partitions: Vec<(String, i64, i64, usize)> =
        agg_json.clone().map(|mut a| extract_partitions(&mut a)).unwrap_or_default();

    // `search_after` names where the previous page ended
    let search_after: Option<Vec<SortValue>> = body
        .get("search_after")
        .and_then(|v| v.as_array())
        .filter(|a| a.len() == sort_keys.len() && !a.is_empty())
        // a marker of nulls names no page at all: it is where a caller starts
        .filter(|a| !a.iter().all(|v| v.is_null()))
        .map(|a| {
            a.iter()
                .zip(sort_keys.iter())
                .map(|(v, k)| sort_value_from_json(v, date_sort_kind(store, &targets, &k.field)))
                .collect()
        });
    let fanned_out = targets.len() > 1;
    let run_shard =
        |shard_idx: usize, name: &String| -> std::result::Result<Option<ShardOut>, Response> {
            search_one_shard(
                store,
                shard_idx,
                name,
                body,
                p,
                &query_json,
                &sort_keys,
                &search_after,
                &pit_ceiling,
                &agg_json,
                &filters_aggs,
                page_want,
                fanned_out,
            )
        };

    let outs: Vec<std::result::Result<Option<ShardOut>, Response>> = if targets.len() > 1 {
        use rayon::prelude::*;
        targets.par_iter().enumerate().map(|(i, n)| run_shard(i, n)).collect()
    } else {
        targets.iter().enumerate().map(|(i, n)| run_shard(i, n)).collect()
    };

    for out in outs {
        let Some(o) = out? else { continue };
        shards += o.shards;
        total += o.count as u64;
        if o.count == 0 {
            empty_shards += 1;
        }
        cands.extend(o.cands);
        if let Some(res) = o.agg {
            fruits.push(res);
        }
        if o.agg_req.is_some() {
            agg_req = o.agg_req;
        }
        if agg_meta.is_empty() {
            agg_meta = o.agg_meta;
        }
        if bucket_orders.is_empty() {
            bucket_orders = o.bucket_orders;
        }
        if let Some(pr) = o.profile {
            shard_profiles.push(pr);
        }
        searchers.push((o.name, o.searcher, o.st));
    }

    // A wide fan-out leaves one intermediate result per index to combine.
    // Folding them one after another is linear and single-threaded, which at
    // a couple of hundred indices is a visible share of the whole request; a
    // tree reduction spreads it over the pool the shards already ran on.
    {
        agg_acc = if fruits.len() > 8 {
            use rayon::prelude::*;
            fruits.into_par_iter().reduce_with(|mut a, b| {
                let _ = a.merge_fruits(b);
                a
            })
        } else {
            fruits.into_iter().reduce(|mut a, b| {
                let _ = a.merge_fruits(b);
                a
            })
        };
    }

    // the order documents arrived in settles a tie, so it has to be known
    // before the page is cut rather than after
    fill_seq(&mut cands, &searchers);
    prune(&mut cands, page_want, &sort_keys);
    // `indices_boost` weights whole indices against each other, so it is
    // applied to the scores before they are ranked. An alias may name the
    // index instead of the index naming itself.
    if let Some(boosts) = body.get("indices_boost") {
        apply_indices_boost(store, &mut cands, &searchers, boosts, p)?;
    }

    // a geo shape, an intervals rule or a distance_feature is settled from the
    // candidates' own values, and what survives is the new total
    if extras.geo || extras.intervals || extras.distance_feature {
        let before = cands.len();
        settle_by_value(&mut cands, &searchers, body, &extras);
        if cands.len() != before {
            total = cands.len() as u64;
        }
    }

    // `rescore` runs a second query over the top of the page and mixes its
    // score into the one already there
    let rescored = apply_rescores(store, &targets, &mut cands, &searchers, body, &sort_keys)?;
    // Where a sort names a filter on the nested objects it reads, only the
    // objects that match it have anything to say. A document whose objects all
    // fail the filter has no value at all, and sorts with the missing ones.
    if nested_filtered {
        sort_by_filtered_nested(store, &targets, &mut cands, &searchers, &sort_keys);
    }
    // `function_score` says what a document's score should be, given what the
    // query scored it and what the document itself holds
    if let Some(spec) = body.pointer("/query/function_score") {
        rescore_by_functions(&searchers, &mut cands, spec)?;
    }
    // `script_score` hands each candidate's score to a script and keeps what
    // it returns; a `min_score` drops those the script rated too low
    if let Some(spec) = body.pointer("/query/script_score") {
        let before = cands.len();
        rescore_by_script(&searchers, &mut cands, spec)?;
        if cands.len() != before {
            total = cands.len() as u64;
        }
    }
    // a rank feature scores by the value of a field, curved the way the query
    // asks for
    if let Some(query) = body.get("query") {
        let mut features = Vec::new();
        collect_rank_features(query, &mut features);
        if !features.is_empty() {
            rescore_by_rank_features(&searchers, &mut cands, &features);
        }
    }

    cands.sort_by(|a, b| cmp_cands(a, b, &sort_keys));

    // a score is only the best score when the ranking is by score descending;
    // any other order makes the top hit's score arbitrary
    let ranked_by_score = sort_keys.is_empty()
        || sort_keys.first().map(|k| k.field == "_score" && k.desc).unwrap_or(false);
    let max_score = if ranked_by_score {
        cands.iter().map(|c| c.score).fold(None::<f32>, |acc, s| Some(acc.map_or(s, |a| a.max(s))))
    } else {
        None
    };

    // A slice takes the shards whose number falls to it. Which shard a
    // document belongs to follows from its id, so the split holds however the
    // documents were spread -- and every slice together covers all of them.
    if let Some(slice) = slice.as_ref() {
        let id = slice.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let max = slice.get("max").and_then(|v| v.as_u64()).unwrap_or(1).max(1);
        cands.retain(|c| {
            let (_, searcher, st) = &searchers[c.shard];
            let g = st.read();
            let shards = g.numeric_setting("number_of_shards").unwrap_or(1).max(1);
            match source_of(searcher, &g, c.addr) {
                Some((doc_id, _)) => {
                    let routed = routing_shard(&doc_id, shards);
                    routed % max == id
                }
                None => false,
            }
        });
        total = cands.len() as u64;
    }

    // `min_score` is the score a document has to reach to be an answer at
    // all: one below it is not a hit, and is not counted as one
    if let Some(floor) = body.get("min_score").and_then(|v| v.as_f64()) {
        cands.retain(|c| c.score as f64 >= floor);
        total = cands.len() as u64;
    }

    // `post_filter` narrows what comes back without narrowing what the
    // aggregations saw, which is the whole point of asking for it
    if let Some(spec) = body.get("post_filter") {
        let keep = matching_ids(store, &targets, spec)?;
        cands.retain(|c| {
            let (_, searcher, st) = &searchers[c.shard];
            let g = st.read();
            source_of(searcher, &g, c.addr).map(|(id, _)| keep.contains(&id)).unwrap_or(false)
        });
        total = cands.len() as u64;
    }

    // `collapse` keeps one hit per distinct value of a field: the best one,
    // which after the sort is the first each value is seen at. It has to run
    // before the page is cut, or a page could be all one value's worth
    if let Some(field) = body.pointer("/collapse/field").and_then(|v| v.as_str()) {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        cands.retain(|c| {
            let (_, searcher, st) = &searchers[c.shard];
            let g = st.read();
            // a field declared as an alias is another name for one that is
            // really in the document
            let real = g.mapping.target_of(field).unwrap_or(field);
            let path = format!("/{}", real.replace('.', "/"));
            let value = source_of(searcher, &g, c.addr)
                .and_then(|(_, src)| src.pointer(&path).cloned())
                .map(|v| match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                });
            match value {
                Some(v) => seen.insert(v),
                // a document with no value there collapses with nothing
                None => true,
            }
        });
    }

    // now, and only now, read stored fields -- for at most `size` documents
    let mut all_hits: Vec<Hit> = Vec::new();
    for c in cands.into_iter().skip(from).take(size) {
        let (name, searcher, st) = &searchers[c.shard];
        let g = st.read();
        let Some((id, mut src)) = source_of(searcher, &g, c.addr) else { continue };
        // `_ignored` travels inside the stored source but belongs on the hit
        let ignored = src.as_object_mut().and_then(|o| o.remove("_ignored"));
        let version = g.version_of(&id);
        all_hits.push(Hit {
            seq: c.seq,
            shard_idx: c.shard,
            index: name.clone(),
            id,
            score: c.score,
            source: src,
            sort: c.sort,
            version,
            ignored,
        });
    }

    // Highlighting a long field means analysing it. Where the index caps how
    // much may be analysed, a field that exceeds the cap is refused rather
    // than silently truncated -- unless the request says how much to analyse,
    // or the field stores offsets and the highlighter can use them.
    if let Some(spec) = body.get("highlight") {
        for h in &all_hits {
            let g = searchers[h.shard_idx].2.read();
            let Some(cap) =
                g.setting("highlight.max_analyzed_offset").and_then(|v| v.parse::<usize>().ok())
            else {
                break;
            };
            let plain = spec.get("type").and_then(|t| t.as_str()) == Some("plain");
            let Some(fields) = spec.get("fields").and_then(|f| f.as_object()) else { break };
            for (name, opts) in fields {
                if opts.get("max_analyzer_offset").is_some() {
                    continue;
                }
                let has_offsets = g
                    .mapping
                    .field_option(name, "index_options")
                    .and_then(|v| v.as_str().map(|s| s == "offsets"))
                    .unwrap_or(false)
                    || g.mapping.field_option(name, "term_vector").is_some();
                if has_offsets && !plain {
                    continue;
                }
                let too_long = h
                    .source
                    .pointer(&format!("/{}", name.replace('.', "/")))
                    .and_then(|v| v.as_str())
                    .map(|t| t.len() > cap)
                    .unwrap_or(false);
                if too_long {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        format!(
                            "The length of [{name}] field of [{}] doc of [{}] index has exceeded \
                             [{cap}] - maximum allowed to be analyzed for highlighting.",
                            h.id, h.index
                        ),
                    ));
                }
            }
        }
    }

    let suggest = match body.get("suggest") {
        Some(spec) => {
            let typed = p.get("typed_keys").map(|v| v != "false").unwrap_or(false);
            Some(build_suggest(store, &targets, spec, typed)?)
        }
        None => None,
    };

    // a clause given a name says so on every hit it matched
    let page_ids: Vec<String> = all_hits.iter().map(|h| h.id.clone()).collect();
    let named = if extras.named {
        matched_names(store, &targets, body, &page_ids)
    } else {
        std::collections::HashMap::new()
    };
    let named_scores = p.get("include_named_queries_score").map(|v| v != "false").unwrap_or(false);
    let mut script_error = None;
    let page = write_page(
        store,
        &targets,
        &searchers,
        all_hits,
        body,
        p,
        &query_json,
        &sort_keys,
        &source_sel,
        &stored,
        &field_specs,
        &named,
        named_scores,
        rescored,
        &extras,
        &mut script_error,
    );
    if let Some(failed) = script_error {
        return Err(failed);
    }

    let filters_results = run_peeled_aggs(store, &targets, &query_json, &filters_aggs, weighted)?;

    let aggs = finalise_aggs(
        store,
        &targets,
        agg_acc,
        agg_req,
        &agg_json,
        &bucket_orders,
        &partitions,
        &agg_meta,
        weighted,
    )?;

    let aggs = if filters_results.is_empty() {
        aggs
    } else {
        let mut base = aggs.unwrap_or_else(|| json!({}));
        for (name, v) in &filters_results {
            base[name.clone()] = v.clone();
        }
        Some(base)
    };

    if p.get("profile").map(|v| v == "true").unwrap_or(false)
        || body.get("profile").and_then(|v| v.as_bool()).unwrap_or(false)
    {
        own_agg_profiles(&filters_aggs, &filters_results, &query_json, &mut shard_profiles);
    }

    // the profile is written while the aggregation runs, before there are any
    // buckets to count, so the count is filled in from the finished answer
    if let (Some(a), false) = (aggs.as_ref(), shard_profiles.is_empty()) {
        for shard in shard_profiles.iter_mut() {
            let Some(entries) = shard.get_mut("aggregations").and_then(|e| e.as_array_mut()) else {
                continue;
            };
            for entry in entries.iter_mut() {
                let Some(name) = entry.get("description").and_then(|d| d.as_str()) else {
                    continue;
                };
                // a bucket that had to be filled in to close a gap was never
                // built while collecting, so it is not one of the buckets the
                // aggregation counts
                let n = a
                    .get(name)
                    .and_then(|v| v.get("buckets"))
                    .and_then(|b| b.as_array())
                    .map(|b| {
                        b.iter()
                            .filter(|x| {
                                x.get("doc_count").and_then(|c| c.as_u64()).unwrap_or(1) > 0
                            })
                            .count()
                    })
                    .unwrap_or(0);
                if let Some(debug) = entry.get_mut("debug").and_then(|d| d.as_object_mut()) {
                    debug.insert("total_buckets".into(), json!(n));
                }
            }
        }
    }

    let aggs = match aggs {
        Some(mut base) if !bucket_pipelines.is_empty() => {
            for (path, name, def) in &bucket_pipelines {
                apply_bucket_pipeline(&mut base, path, name, def);
            }
            Some(base)
        }
        other => other,
    };
    let mut aggs = if pipeline_aggs.is_empty() {
        aggs
    } else {
        let mut base = aggs.unwrap_or_else(|| json!({}));
        for (name, def) in pipeline_aggs {
            base[name] = run_pipeline_agg(&base, &def)?;
        }
        Some(base)
    };

    if let Some(a) = aggs.as_mut() {
        millis_in_keys(a);
    }

    if let (Some(a), Some(req)) =
        (aggs.as_mut(), body.get("aggs").or_else(|| body.get("aggregations")))
    {
        keep_asked_ranges(req, a);
    }

    if let (Some(a), Some(req)) =
        (aggs.as_mut(), body.get("aggs").or_else(|| body.get("aggregations")))
    {
        name_date_metrics(store, &targets, req, a);
    }

    // `search.max_buckets` caps how many buckets one request may build. The
    // limit is counted over the whole answer, sub-buckets included, which is
    // what makes a nested terms aggregation the expensive one.
    check_max_buckets(store, &aggs)?;

    let agg_forces_all = body
        .get("aggs")
        .or_else(|| body.get("aggregations"))
        .map(needs_all_shards)
        .unwrap_or(false);

    let skipped =
        if p.contains_key("pre_filter_shard_size") && query_json.is_some() && !agg_forces_all {
            empty_shards.min((targets.len() as u64).saturating_sub(1))
        } else {
            0
        };

    // `typed_keys` asks for every aggregation and suggestion to be named after
    // what it produced as well as what it was called
    let (aggs, suggest) = if p.get("typed_keys").map(|v| v != "false").unwrap_or(false) {
        let mut aggs = aggs;
        if let (Some(a), Some(req)) =
            (aggs.as_mut(), body.get("aggs").or_else(|| body.get("aggregations")))
        {
            apply_typed_keys(store, &targets, a, req);
        }
        let mut suggest = suggest;
        if let (Some(sg), Some(req)) = (suggest.as_mut(), body.get("suggest")) {
            apply_typed_keys_suggest(sg, req);
        }
        (aggs, suggest)
    } else {
        (aggs, suggest)
    };

    // `profile` also asks what the fetch cost: reading each hit back, and the
    // sub-phases that filled it in
    if !shard_profiles.is_empty() {
        let nanos = started.elapsed().as_nanos().max(1) as u64;
        fetch_profiles(&mut shard_profiles, body, &extras, &named, size, page.len() as u64, nanos);
    }

    let mut page = page;
    if !join_inner_hits.is_empty() {
        attach_join_inner_hits(store, &targets, &mut page, &join_inner_hits);
    }
    Ok(Outcome {
        took_ms: started.elapsed().as_millis() as u64,
        skipped,
        shards: shards.max(1),
        total,
        hits: page,
        max_score,
        aggs,
        profile: (!shard_profiles.is_empty()).then(|| json!({"shards": shard_profiles})),
        suggest,
        failures,
    })
}
