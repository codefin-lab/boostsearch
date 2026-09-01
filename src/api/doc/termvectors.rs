//! What a document's text became, term by term.

use super::*;

/// `_termvectors` -- what a document's text became once analysed.
///
/// The terms are recovered by analysing the stored source again rather than
/// from a second index of offsets, which is the same ground the highlighter
/// stands on. Document frequency is counted against the index, so it is the
/// real one rather than a guess.
pub async fn termvectors(
    State(store): State<Store>,
    path: Path<Vec<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let parts = path.0;
    let index = parts.first().cloned().unwrap_or_default();
    let id = parts.get(1).cloned();
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let Some(st) = store.get(&index) else { return no_such_index(&index) };
    let g = st.read();
    let id = id.or_else(|| body.get("_id").and_then(|v| v.as_str().map(|s| s.into())));
    let Some(id) = id else {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: id is missing;",
        );
    };
    let source = read_source_as_asked(&g, &id, &p);
    let Some(source) = source else {
        return respond(
            &p,
            json!({
                "_index": g.name, "_id": id, "_version": 0, "found": false, "took": 0,
            }),
        );
    };
    let want_stats = body
        .get("term_statistics")
        .and_then(|v| v.as_bool())
        .or_else(|| p.get("term_statistics").map(|v| v == "true"))
        .unwrap_or(false);
    // what a field holds across the index is reported unless it is refused;
    // what each term is worth is reported only when it is asked for
    let want_field_stats = body
        .get("field_statistics")
        .and_then(|v| v.as_bool())
        .or_else(|| p.get("field_statistics").map(|v| v != "false"))
        .unwrap_or(true);
    let only: Option<Vec<String>> = body
        .get("fields")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .or_else(|| p.get("fields").map(|f| f.split(',').map(|s| s.trim().to_string()).collect()));

    let fields = term_vectors_of(&g, &source, want_stats, want_field_stats, only.as_deref());
    respond(
        &p,
        json!({
            "_index": g.name, "_id": id, "_version": g.version_of(&id),
            "found": true, "took": 0, "term_vectors": fields,
        }),
    )
}

/// The terms each field of a document became once analysed.
pub(crate) fn term_vectors_of(
    g: &IdxState,
    source: &Value,
    want_stats: bool,
    want_field_stats: bool,
    only: Option<&[String]>,
) -> Value {
    let mut fields = serde_json::Map::new();
    let Some(obj) = source.as_object() else { return Value::Object(fields) };
    for (name, value) in obj {
        if only.map(|f| !f.iter().any(|w| w == name)).unwrap_or(false) {
            continue;
        }
        let Some(text) = value.as_str() else { continue };
        let spans = crate::query::analyze_spans(&g.index, text, None);
        if spans.is_empty() {
            continue;
        }
        // group the occurrences by the term they are of
        let mut terms: std::collections::BTreeMap<String, Vec<(usize, usize, usize)>> =
            std::collections::BTreeMap::new();
        for (t, pos, from, to) in spans {
            terms.entry(t).or_default().push((pos, from, to));
        }
        let searcher = g.reader.searcher();
        let mut out = serde_json::Map::new();
        let mut sum_doc_freq = 0u64;
        let mut sum_ttf = 0u64;
        for (term, spots) in &terms {
            let mut entry = json!({
                "term_freq": spots.len(),
                "tokens": spots.iter().map(|(pos, from, to)| json!({
                    "position": pos, "start_offset": from, "end_offset": to,
                })).collect::<Vec<_>>(),
            });
            if want_stats {
                // how many documents hold this term, counted rather than assumed
                let ctx = crate::query::Ctx {
                    fields: &g.fields,
                    mapping: &g.mapping,
                    analysis: &g.analysis,
                    index: &g.index,
                    max_terms_count: g.max_terms_count(),
                    max_regex_length: g.max_regex_length(),
                    allow_expensive: true,
                    observed_kinds: &g.observed_kinds,
                    kinds_complete: g.kinds_complete,
                    stats: &g.stats,
                };
                let freq = crate::query::build(&ctx, &json!({"match": {name.clone(): term}}))
                    .ok()
                    .and_then(|q| searcher.search(&q, &boostcore::collector::Count).ok())
                    .unwrap_or(1) as u64;
                entry["doc_freq"] = json!(freq);
                entry["ttf"] = json!(spots.len());
                sum_doc_freq += freq;
                sum_ttf += spots.len() as u64;
            }
            out.insert(term.clone(), entry);
        }
        let mut field = json!({"terms": Value::Object(out)});
        if want_field_stats {
            field["field_statistics"] = json!({
                "sum_doc_freq": sum_doc_freq,
                "doc_count": searcher.num_docs(),
                "sum_ttf": sum_ttf,
            });
        }
        fields.insert(name.clone(), field);
    }
    Value::Object(fields)
}

/// `_mtermvectors` -- term vectors for several documents at once.
pub async fn mtermvectors(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let default_index = index.map(|Path(i)| i);
    let docs: Vec<Value> = match body.get("docs").and_then(|v| v.as_array()) {
        Some(a) => a.clone(),
        None => {
            // `ids` may be written in the body or on the URL
            let listed: Vec<Value> = body
                .get("ids")
                .and_then(|v| v.as_array())
                .cloned()
                .or_else(|| {
                    p.get("ids")
                        .filter(|v| !v.is_empty())
                        .map(|v| v.split(',').map(|s| json!(s.trim())).collect())
                })
                .unwrap_or_default();
            listed.into_iter().map(|id| json!({"_id": id})).collect()
        }
    };
    // the camel-cased spellings, and the ones that moved onto the request,
    // are no longer read from a document
    for d in &docs {
        for gone in ["version", "versionType", "version_type", "_version", "routing", "_routing"] {
            if d.get(gone).is_some() {
                return err(
                    StatusCode::BAD_REQUEST,
                    "action_request_validation_exception",
                    format!("Validation Failed: 1: unknown field [{gone}];"),
                );
            }
        }
    }
    let mut out = Vec::new();
    for d in docs {
        let idx = d
            .get("_index")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .or_else(|| default_index.clone())
            .unwrap_or_default();
        let id = d.get("_id").map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        });
        let Some(id) = id else { continue };
        let Some(st) = store.get(&idx) else {
            let reason = format!("no such index [{idx}]");
            let cause = json!({
                "type": "index_not_found_exception", "reason": reason,
                "index": idx, "resource.type": "index_expression",
                "resource.id": idx, "index_uuid": "_na_",
            });
            let mut error = json!({
                "type": "index_not_found_exception", "reason": reason,
                "index": idx, "resource.type": "index_expression",
                "resource.id": idx, "index_uuid": "_na_",
                "root_cause": [cause],
            });
            add_stack_trace(&mut error, &p, "mtermvectors");
            out.push(json!({"_index": idx, "_id": id, "found": false, "error": error}));
            continue;
        };
        let g = st.read();
        match read_source_as_asked(&g, &id, &p) {
            Some(src) => {
                let want_stats = body
                    .get("term_statistics")
                    .and_then(|v| v.as_bool())
                    .or_else(|| p.get("term_statistics").map(|v| v == "true"))
                    .unwrap_or(false);
                out.push(json!({
                    "_index": g.name, "_id": id, "_version": g.version_of(&id),
                    "found": true, "took": 0,
                    "term_vectors": term_vectors_of(&g, &src, want_stats, true, None),
                }))
            }
            None => out.push(json!({
                "_index": g.name, "_id": id, "found": false, "took": 0,
            })),
        }
    }
    respond(&p, json!({"docs": out}))
}
