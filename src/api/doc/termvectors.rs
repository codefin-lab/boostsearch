//! What a document's text became, term by term.

use super::*;
use velocore::DocSet;
use velocore::postings::Postings;
use velocore::query::Bm25StatisticsProvider;
use velocore::schema::IndexRecordOption;

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
    // a document named by id is looked for on the shard its routing names,
    // which a mapping requiring routing will not guess at
    if parts.get(1).is_some()
        && let Some(refusal) = read_routing_refusal(&g, &id, &p)
    {
        return refusal;
    }
    let source = read_source_as_asked(&g, &id, &p)
        .filter(|_| routing_matches(&g, &id, &p))
        .filter(|_| crate::security::doc_visible(&store, &g, &id));
    let Some(source) = source else {
        return respond(
            &p,
            json!({
                "_index": g.name, "_id": id, "_version": 0, "found": false, "took": 0,
            }),
        );
    };
    // what a field holds across the index is reported unless it is refused;
    // what each term is worth is reported only when it is asked for
    let shape = Shape::of(&body, &p);
    let only: Option<Vec<String>> = body
        .get("fields")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .or_else(|| p.get("fields").map(|f| f.split(',').map(|s| s.trim().to_string()).collect()));

    let mut fields = term_vectors_of(&g, &source, shape, only.as_deref());
    crate::security::narrow_term_vectors(&store, &g.name, &mut fields);
    respond(
        &p,
        json!({
            "_index": g.name, "_id": id, "_version": g.version_of(&id),
            "found": true, "took": 0, "term_vectors": fields,
        }),
    )
}

/// The terms each field of a document became once analysed.
/// What the index holds for one field: how many documents each of its terms
/// is in, added up; how many times they stand there, added up; and how many
/// documents hold the field.
fn field_statistics_of(g: &IdxState, field: &str) -> (u64, u64, u64) {
    let searcher = g.reader.searcher();
    // Which field of the index holds this one's terms, and under which path:
    // a field the mapping declares lives in the untouched view rather than
    // among the dynamic JSON, and asking the dynamic field for it found
    // nothing -- `sum_doc_freq` came back zero for a keyword every document
    // had, where the reference reports one for each of them.
    let (held, path) = held_path(g, field);
    let mut start = velocore::Term::from_field_json_path(held, &path, true);
    start.append_type_and_str("");
    let prefix = start.serialized_value_bytes().to_vec();
    let mut sum_doc_freq = 0u64;
    // `sum_ttf` is every token the field holds, which is every occurrence of
    // every term: it was the sum of the term frequencies of this one
    // document, so a field of 402 tokens across the index reported 330
    let mut sum_ttf = 0u64;
    for reader in searcher.segment_readers() {
        let Ok(inverted) = reader.inverted_index(held) else { continue };
        let Ok(mut stream) = inverted.terms().stream() else { continue };
        while let Some((bytes, info)) = stream.next() {
            if bytes.starts_with(&prefix) {
                sum_doc_freq += info.doc_freq as u64;
                if let Ok(mut postings) =
                    inverted.read_postings_from_terminfo(info, IndexRecordOption::WithFreqs)
                {
                    while postings.doc() != velocore::TERMINATED {
                        sum_ttf += postings.term_freq() as u64;
                        postings.advance();
                    }
                }
            }
        }
    }
    let docs = searcher.path_statistics(&start).map(|(docs, _)| docs).filter(|d| *d > 0);
    (sum_doc_freq, sum_ttf, docs.unwrap_or_else(|| searcher.num_docs()))
}

/// The field of the index a field's terms are kept in, and the path under it.
fn held_path(g: &IdxState, field: &str) -> (velocore::schema::Field, String) {
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
        vectors: &g.vectors,
    };
    let (held, path, _) = ctx.resolve(field, false);
    (held, path.replace('.', "\u{1}"))
}

/// What of each token a term vector reports.
#[derive(Clone, Copy)]
pub(crate) struct Shape {
    pub term_statistics: bool,
    pub field_statistics: bool,
    pub positions: bool,
    pub offsets: bool,
}

impl Shape {
    pub(crate) fn of(body: &Value, p: &Params) -> Shape {
        let flag = |key: &str, default: bool| {
            body.get(key)
                .and_then(|v| v.as_bool())
                .or_else(|| p.get(key).map(|v| v != "false"))
                .unwrap_or(default)
        };
        Shape {
            term_statistics: flag("term_statistics", false),
            field_statistics: flag("field_statistics", true),
            positions: flag("positions", true),
            offsets: flag("offsets", true),
        }
    }
}

pub(crate) fn term_vectors_of(
    g: &IdxState,
    source: &Value,
    shape: Shape,
    only: Option<&[String]>,
) -> Value {
    let mut fields = serde_json::Map::new();
    let Some(obj) = source.as_object() else { return Value::Object(fields) };
    // The fields asked for by name are looked up by name, a multi-field
    // included: `body.english` is not a key of the source, it is `body` cut
    // by another analyzer, and asking for it answered nothing at all.
    let named: Vec<(String, String)> = match only {
        Some(asked) => asked
            .iter()
            .filter_map(|name| {
                let pointer = |n: &str| format!("/{}", n.replace('.', "/"));
                if let Some(text) = source.pointer(&pointer(name)).and_then(|v| v.as_str()) {
                    return Some((name.clone(), text.to_string()));
                }
                let (parent, _) = name.rsplit_once('.')?;
                g.mapping.type_of(name)?;
                let text = source.pointer(&pointer(parent))?.as_str()?;
                Some((name.clone(), text.to_string()))
            })
            .collect(),
        None => obj
            .iter()
            .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_string())))
            .collect(),
    };
    for (name, text) in named {
        let text = text.as_str();
        // a keyword holds its whole value as one term; anything else is cut
        // by the analyzer the field was written with
        let chain = g
            .mapping
            .field_option(&name, "analyzer")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .and_then(|named| g.analysis.get(&named));
        let mut spans = if g.mapping.type_of(&name) == Some("keyword") {
            vec![(text.to_string(), 0usize, 0usize, text.len(), 1usize)]
        } else {
            match &chain {
                Some(chain) => chain.tokens(text),
                None => crate::query::analyze_spans(&g.index, text, None),
            }
        };
        // the offsets go out as Java counts them, not as bytes
        crate::analysis::reported_offsets(text, &mut spans);
        // a chain that hangs the token's kind on it as a payload has it read
        // back here, and the kind of a word is `<ALPHANUM>`
        let payload = chain
            .map(|chain| chain.carries_type_payload())
            .unwrap_or(false)
            .then_some("PEFMUEhBTlVNPg==");
        if spans.is_empty() {
            continue;
        }
        // group the occurrences by the term they are of
        let mut terms: std::collections::BTreeMap<String, Vec<(usize, usize, usize)>> =
            std::collections::BTreeMap::new();
        for (t, pos, from, to, _) in spans {
            terms.entry(t).or_default().push((pos, from, to));
        }
        let searcher = g.reader.searcher();
        let (held, path) = held_path(g, &name);
        let mut out = serde_json::Map::new();
        for (term, spots) in &terms {
            let mut entry = json!({ "term_freq": spots.len() });
            if shape.positions || shape.offsets {
                entry["tokens"] = spots
                    .iter()
                    .map(|(pos, from, to)| {
                        let mut token = json!({});
                        if shape.positions {
                            token["position"] = json!(pos);
                        }
                        if shape.offsets {
                            token["start_offset"] = json!(from);
                            token["end_offset"] = json!(to);
                        }
                        if let Some(payload) = payload {
                            token["payload"] = json!(payload);
                        }
                        token
                    })
                    .collect::<Vec<_>>()
                    .into();
            }
            if shape.term_statistics {
                // how many documents hold this term, and how many times it
                // stands in all of them -- `ttf` was this document's own
                // count, so a term twice here and fifteen times in the index
                // reported two
                let mut exact = velocore::Term::from_field_json_path(held, &path, true);
                exact.append_type_and_str(term);
                let mut doc_freq = 0u64;
                let mut ttf = 0u64;
                for reader in searcher.segment_readers() {
                    let Ok(inverted) = reader.inverted_index(held) else { continue };
                    let Ok(Some(mut postings)) =
                        inverted.read_postings(&exact, IndexRecordOption::WithFreqs)
                    else {
                        continue;
                    };
                    while postings.doc() != velocore::TERMINATED {
                        doc_freq += 1;
                        ttf += postings.term_freq() as u64;
                        postings.advance();
                    }
                }
                entry["doc_freq"] = json!(doc_freq.max(1));
                entry["ttf"] = json!(ttf.max(spots.len() as u64));
            }
            out.insert(term.clone(), entry);
        }
        let mut field = json!({"terms": Value::Object(out)});
        if shape.field_statistics {
            // the field's statistics are the index's, whatever this one
            // document holds of it
            let (sum_doc_freq, sum_ttf, doc_count) = field_statistics_of(g, &name);
            field["field_statistics"] = json!({
                "sum_doc_freq": sum_doc_freq,
                "doc_count": doc_count,
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
        // A document with no id is not a document to skip: the answers are
        // paired with the requests by position, and dropping one shifted
        // every answer after it onto the wrong request. It is refused, the
        // way `_mget` refuses the same thing.
        let Some(id) = id else {
            return err(
                StatusCode::BAD_REQUEST,
                "action_request_validation_exception",
                "Validation Failed: 1: id is missing;",
            );
        };
        if let Some(why) = crate::security::item_refusal(
            &store,
            &["indices:data/read/tv"],
            &crate::security::layer::indices_for_expr(&store, &idx),
        ) {
            out.push(json!({"_index": idx, "_id": id, "error": crate::security::item_error(&why)}));
            continue;
        }
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
        match read_source_as_asked(&g, &id, &p)
            .filter(|_| crate::security::doc_visible(&store, &g, &id))
        {
            Some(src) => {
                let mut shape = Shape::of(&d, &p);
                shape.term_statistics = body
                    .get("term_statistics")
                    .and_then(|v| v.as_bool())
                    .or_else(|| d.get("term_statistics").and_then(|v| v.as_bool()))
                    .or_else(|| p.get("term_statistics").map(|v| v == "true"))
                    .unwrap_or(false);
                let mut tv = term_vectors_of(&g, &src, shape, None);
                crate::security::narrow_term_vectors(&store, &g.name, &mut tv);
                out.push(json!({
                    "_index": g.name, "_id": id, "_version": g.version_of(&id),
                    "found": true, "took": 0,
                    "term_vectors": tv,
                }))
            }
            None => out.push(json!({
                "_index": g.name, "_id": id, "found": false, "took": 0,
            })),
        }
    }
    respond(&p, json!({"docs": out}))
}
