//! The page of hits, dressed the way the request asked for it.

use super::*;

pub(crate) fn source_of(
    searcher: &Searcher,
    st: &IdxState,
    addr: DocAddress,
) -> Option<(String, Value)> {
    let doc: TantivyDocument = searcher.doc(addr).ok()?;
    let id = doc.get_first(st.fields.id)?.as_str()?.to_string();
    let raw = doc.get_first(st.fields.source)?.as_str()?.to_string();
    let src = serde_json::from_str(&raw).ok()?;
    Some((id, src))
}

/// Write the page of hits the client reads.
///
/// Everything expensive has happened by now: these are the documents that made
/// the page, and this is where each one is dressed -- source selection, the
/// values `fields` asked for, inner hits, highlighting, the names of the
/// clauses it matched.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_page(
    store: &Store,
    targets: &[String],
    searchers: &Searchers,
    all_hits: Vec<Hit>,
    body: &Value,
    p: &Params,
    query_json: &Option<Value>,
    sort_keys: &[SortKey],
    source_sel: &Option<Value>,
    stored: &Option<Vec<String>>,
    field_specs: &Option<Vec<(String, Option<String>)>>,
    named: &std::collections::HashMap<String, Vec<(String, f32)>>,
    named_scores: bool,
    rescored: bool,
    extras: &Extras,
) -> Vec<Value> {
    all_hits
        .into_iter()
        .map(|h| {
            // `stored_fields: _none_` strips the metadata too
            let none = stored.as_ref().map(|s| s.is_empty()).unwrap_or(false)
                && body.get("stored_fields").map(|v| v == "_none_").unwrap_or(false);
            let mut hit = if none {
                json!({"_score": if sort_keys.is_empty() { json!(h.score) } else { Value::Null }})
            } else {
                json!({
                    "_index": h.index,
                    "_id": h.id,
                    "_score": if sort_keys.is_empty() { json!(h.score) } else { Value::Null },
                })
            };
            // a selector on the URL is the narrower instruction and wins over
            // one in the body
            let sel = crate::api::source_selector_from_params_pub(p).or_else(|| source_sel.clone());
            let explicit_source = sel.is_some();
            if let Some(names) = &stored {
                let mut out = serde_json::Map::new();
                for name in names.iter().filter(|n| *n != "_source") {
                    if let Some(v) = h.source.pointer(&format!("/{}", name.replace('.', "/"))) {
                        out.insert(
                            name.clone(),
                            match v {
                                Value::Array(a) => Value::Array(a.clone()),
                                other => Value::Array(vec![other.clone()]),
                            },
                        );
                    }
                }
                if !out.is_empty() {
                    hit["fields"] = Value::Object(out);
                }
            }
            // `stored_fields` suppresses `_source` unless it was asked for too,
            // and a mapping may say the document is not kept at all
            let kept = searchers[h.shard_idx].2.read().mapping.raw.pointer("/_source/enabled")
                != Some(&json!(false));
            let want_source = kept
                && (stored.is_none()
                    || explicit_source
                    || stored.as_ref().map(|s| s.iter().any(|n| n == "_source")).unwrap_or(false));
            if want_source {
                let src = match &sel {
                    Some(s) => apply_source_selector(&h.source, s),
                    None => h.source.clone(),
                };
                if !src.is_null() {
                    hit["_source"] = src;
                }
            }
            if let Some(ig) = &h.ignored {
                hit["_ignored"] = ig.clone();
            }
            // `explain` asks where the score came from. What can be said here
            // is the score itself and what it was arrived at by.
            if body.get("explain").and_then(|v| v.as_bool()).unwrap_or(false) {
                let description = if rescored {
                    "sum of the query score and the rescoring query score"
                } else if sort_keys.is_empty() {
                    "score of the query"
                } else {
                    "the query matched; the order comes from the sort"
                };
                hit["_explanation"] = json!({
                    "value": h.score,
                    "description": description,
                    "details": [],
                });
            }
            if !h.sort.is_empty() {
                // the column holds the number the field reports -- a date is
                // milliseconds, a date_nanos is nanoseconds -- so a sort value
                // goes out as it was read
                hit["sort"] = Value::Array(h.sort.iter().map(|s| s.to_json()).collect());
            }
            if let Some(specs) = field_specs.as_ref() {
                let g = searchers[h.shard_idx].2.read();
                // a flat_object is one value unless the request named a path
                // inside it, in which case it has to be descended
                let is_leaf = |p: &str| {
                    g.mapping.is_leaf_type(p)
                        && !specs.iter().any(|(n, _)| {
                            n.len() > p.len() && n.starts_with(p) && n.as_bytes()[p.len()] == b'.'
                        })
                };
                // a field without doc values has nothing for `fields` to read
                let names: Vec<String> = specs
                    .iter()
                    .map(|(n, _)| n.clone())
                    .filter(|n| g.mapping.field_option(n, "doc_values") != Some(json!(false)))
                    .collect();
                let raw = crate::source::extract_fields(&h.source, &names, &is_leaf);
                // A field may be asked for more than once, each time with its
                // own format, and each asking adds its values to the one list
                // the field is reported under.
                let mut f = serde_json::Map::new();
                for (name, fmt) in specs.iter() {
                    let mut values = match name.as_str() {
                        // the metadata a document carries is asked for the same
                        // way as its own fields, and is not in the source
                        "_seq_no" => json!([h.seq]),
                        "_index" => json!([h.index.clone()]),
                        "_id" => json!([h.id.clone()]),
                        _ => match raw.get(name) {
                            Some(v) => v.clone(),
                            None => continue,
                        },
                    };
                    if let (Some(fmt), Value::Array(items)) = (fmt, &mut values) {
                        for v in items.iter_mut() {
                            if let Some(text) = crate::source::format_date(v, fmt) {
                                *v = text;
                            } else if let Some(n) = v.as_f64() {
                                if let Some(text) = decimal_format(fmt, n) {
                                    *v = json!(text);
                                }
                            }
                        }
                    }
                    match (f.get_mut(name.as_str()), values) {
                        (Some(Value::Array(into)), Value::Array(more)) => into.extend(more),
                        (_, values) => {
                            f.insert(name.clone(), values);
                        }
                    }
                }
                // a token_count field stores the text but reports the count
                for (name, vals) in f.iter_mut() {
                    if g.mapping.type_of(name) != Some("token_count") {
                        continue;
                    }
                    if let Value::Array(items) = vals {
                        for v in items.iter_mut() {
                            if let Some(t) = v.as_str() {
                                *v = json!(crate::store::token_count(t));
                            }
                        }
                    }
                }
                // a value the index refused is not a value the field has
                if let Some(Value::Array(ig)) = &h.ignored {
                    for name in ig.iter().filter_map(|v| v.as_str()) {
                        f.remove(name);
                    }
                }
                // `stored_fields` may have filled some in already; both
                // selections share the one `fields` section
                if let Some(Value::Object(existing)) = hit.get("fields") {
                    for (k, v) in existing {
                        f.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
                if !f.is_empty() {
                    hit["fields"] = Value::Object(f);
                }
            }
            // a nested query may ask for the objects it matched to be listed,
            // and a nested query inside one asks the same of the objects under
            // those
            if extras.nested_inner_hits {
                let mut clauses = Vec::new();
                if let Some(q) = body.get("query") {
                    collect_nested_inner_hits(q, &mut clauses);
                }
                if !clauses.is_empty() {
                    let g = searchers[h.shard_idx].2.read();
                    let kept = g.mapping.raw.pointer("/_source/enabled") != Some(&json!(false));
                    let groups = nested_inner_hits(
                        &h, &h.source, "", &clauses, kept, query_json, &g.mapping, &g.index,
                    );
                    if !groups.is_empty() {
                        hit["inner_hits"] = Value::Object(groups);
                    }
                }
            }
            if let Some(hits) = named.get(&h.id) {
                hit["matched_queries"] = if named_scores {
                    let mut m = serde_json::Map::new();
                    for (n, s) in hits {
                        m.insert(n.clone(), json!(s));
                    }
                    Value::Object(m)
                } else {
                    let mut names: Vec<String> = hits.iter().map(|(n, _)| n.clone()).collect();
                    names.sort();
                    json!(names)
                };
            }
            // a collapsed hit says which value it stands for, and may carry
            // the group it was chosen from
            if let Some(field) = body.pointer("/collapse/field").and_then(|v| v.as_str()) {
                let real = searchers[h.shard_idx]
                    .2
                    .read()
                    .mapping
                    .target_of(field)
                    .unwrap_or(field)
                    .to_string();
                let path = format!("/{}", real.replace('.', "/"));
                if let Some(v) = h.source.pointer(&path) {
                    let list = match v {
                        Value::Array(a) => a.clone(),
                        other => vec![other.clone()],
                    };
                    let mut f = match hit.get("fields") {
                        Some(Value::Object(o)) => o.clone(),
                        _ => serde_json::Map::new(),
                    };
                    f.insert(field.to_string(), Value::Array(list.clone()));
                    hit["fields"] = Value::Object(f);
                    // a hit may be asked for its group more than once, each
                    // time with a different name and ordering
                    let asked = match body.pointer("/collapse/inner_hits") {
                        Some(Value::Array(a)) => a.clone(),
                        Some(other) => vec![other.clone()],
                        None => Vec::new(),
                    };
                    let mut groups = serde_json::Map::new();
                    for inner in &asked {
                        let Some(group) = collapsed_group(
                            store,
                            targets,
                            query_json.as_ref(),
                            field,
                            list.first().unwrap_or(&Value::Null),
                            inner,
                            p,
                        ) else {
                            continue;
                        };
                        let name =
                            inner.get("name").and_then(|n| n.as_str()).unwrap_or("inner_hits");
                        groups.insert(name.to_string(), group);
                    }
                    if !groups.is_empty() {
                        hit["inner_hits"] = Value::Object(groups);
                    }
                }
            }
            if let Some(spec) = body.get("highlight") {
                let g = searchers[h.shard_idx].2.read();
                if let Some(hl) = build_highlight(spec, &h.source, query_json, &g.mapping, &g.index)
                {
                    hit["highlight"] = hl;
                }
            }
            if body.get("version").and_then(|v| v.as_bool()).unwrap_or(false) {
                hit["_version"] = json!(h.version);
            }
            if body_or_param(body, p, "seq_no_primary_term")
                .map(|v| v == json!(true) || v == json!("true"))
                .unwrap_or(false)
            {
                hit["_seq_no"] = json!(h.seq);
                hit["_primary_term"] = json!(1);
            }
            hit
        })
        .collect()
}

/// Read them off the request, complaining where the request asks for something
/// the mapping cannot answer.
pub(crate) fn output_specs(
    store: &Store,
    targets: &[String],
    body: &Value,
    p: &Params,
) -> std::result::Result<OutputSpecs, Response> {
    // `fields` reads values back out of the stored source; without one there
    // is nothing to read, and a date format asks a field that holds no dates
    // to answer in a shape it has no values for
    if let Some(specs) = body.get("fields").and_then(|v| v.as_array()) {
        for name in targets.iter() {
            let Some(st) = store.get(name) else { continue };
            let g = st.read();
            if g.mapping.raw.pointer("/_source/enabled") == Some(&json!(false)) {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!(
                        "Unable to retrieve the requested [fields] since _source is disabled \
                         in the mappings for index [{name}]"
                    ),
                ));
            }
            for spec in specs {
                let (Some(f), Some(_)) =
                    (spec.get("field").and_then(|v| v.as_str()), spec.get("format"))
                else {
                    continue;
                };
                if !matches!(
                    g.mapping.type_of(f),
                    None | Some("date" | "date_nanos" | "date_range")
                ) {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        format!("error fetching [{f}]: field has no date formatter"),
                    ));
                }
            }
        }
    }
    // a spec is either a bare field name or an object naming a format
    let spec_list = |v: Option<&Value>| -> Option<Vec<(String, Option<String>)>> {
        v.and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .filter_map(|x| match x {
                    Value::String(s) => Some((s.clone(), None)),
                    Value::Object(o) => o.get("field").and_then(|f| f.as_str()).map(|s| {
                        (
                            s.to_string(),
                            o.get("format").and_then(|f| f.as_str()).map(|s| s.to_string()),
                        )
                    }),
                    _ => None,
                })
                .collect()
        })
    };
    // `docvalue_fields` may also be named on the URL, as a comma-separated list
    let param_docvalues: Option<Value> = p
        .get("docvalue_fields")
        .filter(|v| !v.is_empty())
        .map(|v| Value::Array(v.split(',').map(|f| json!(f.trim())).collect()));
    let body_docvalues = body.get("docvalue_fields").cloned().or(param_docvalues);
    let fields = match (spec_list(body.get("fields")), spec_list(body_docvalues.as_ref())) {
        (Some(mut a), Some(b)) => {
            a.extend(b);
            Some(a)
        }
        (a, b) => a.or(b),
    };
    let stored: Option<Vec<String>> = match body.get("stored_fields") {
        Some(Value::Array(a)) => {
            Some(a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        }
        Some(Value::String(s)) if s == "_none_" => Some(vec![]),
        Some(Value::String(s)) => Some(vec![s.clone()]),
        _ => None,
    };
    Ok(OutputSpecs { source: body.get("_source").cloned(), fields, stored })
}

/// Every clause of a query that was given a `_name`, paired with the clause
/// itself so it can be asked about one document at a time.
pub(crate) fn named_clauses(node: &Value, out: &mut Vec<(String, Value)>) {
    match node {
        Value::Object(o) => {
            for (k, v) in o {
                if k == "_name" {
                    continue;
                }
                let named = v
                    .get("_name")
                    .and_then(|n| n.as_str())
                    // `match: {field: {query, _name}}` puts the name beside the
                    // field's options rather than beside the clause
                    .or_else(|| {
                        v.as_object()
                            .filter(|o| o.len() == 1)
                            .and_then(|o| o.values().next())
                            .and_then(|inner| inner.get("_name"))
                            .and_then(|n| n.as_str())
                    });
                if let Some(name) = named {
                    out.push((name.to_string(), json!({k.clone(): v.clone()})));
                }
                named_clauses(v, out);
            }
        }
        Value::Array(a) => {
            for v in a {
                named_clauses(v, out);
            }
        }
        _ => {}
    }
}

/// Take the `_name` markers out of a clause.
pub(crate) fn strip_names(node: &mut Value) {
    match node {
        Value::Object(o) => {
            o.remove("_name");
            for (_, v) in o.iter_mut() {
                strip_names(v);
            }
        }
        Value::Array(a) => {
            for v in a {
                strip_names(v);
            }
        }
        _ => {}
    }
}

/// Which named clauses each document on the page matched, and with what score.
pub(crate) fn matched_names(
    store: &Store,
    targets: &[String],
    body: &Value,
    ids: &[String],
) -> std::collections::HashMap<String, Vec<(String, f32)>> {
    let mut out: std::collections::HashMap<String, Vec<(String, f32)>> =
        std::collections::HashMap::new();
    let mut clauses = Vec::new();
    if let Some(q) = body.get("query") {
        named_clauses(q, &mut clauses);
    }
    for r in body.get("rescore").into_iter().flat_map(|r| match r {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    }) {
        named_clauses(&r, &mut clauses);
    }
    if clauses.is_empty() || ids.is_empty() {
        return out;
    }
    for (name, mut clause) in clauses {
        // the clause is asked about on its own, and must not carry the name
        // that would make it a named clause all over again
        strip_names(&mut clause);
        let probe = json!({
            "query": {"bool": {"must": [clause], "filter": [{"terms": {"_id": ids}}]}},
            "size": ids.len(),
        });
        let Ok(answer) = run(store, &targets.join(","), &probe, &Params::new()) else { continue };
        for hit in answer.hits {
            let Some(id) = hit.get("_id").and_then(|v| v.as_str()) else { continue };
            let score = hit.get("_score").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
            out.entry(id.to_string()).or_default().push((name.clone(), score));
        }
    }
    out
}

/// The documents a collapsed hit was chosen from: the same query, narrowed to
/// the one value, asked again with whatever the `inner_hits` clause says.
pub(crate) fn collapsed_group(
    store: &Store,
    targets: &[String],
    query: Option<&Value>,
    field: &str,
    value: &Value,
    inner: &Value,
    p: &Params,
) -> Option<Value> {
    // the value the group stands for narrows it; the query that found the
    // group still scores it, which is what decides the order inside
    let mut group = json!({"bool": {"filter": [{"term": {field: value.clone()}}]}});
    if let Some(q) = query {
        group["bool"]["must"] = json!([q.clone()]);
    }
    let mut body = json!({"query": group});
    for key in [
        "size",
        "from",
        "sort",
        "_source",
        "version",
        "seq_no_primary_term",
        "docvalue_fields",
        "stored_fields",
        "highlight",
        "explain",
        "fields",
        // the group may be collapsed again, on a field of its own
        "collapse",
    ] {
        if let Some(v) = inner.get(key) {
            body[key] = v.clone();
        }
    }
    let out = run(store, &targets.join(","), &body, &Params::new()).ok()?;
    let total = if p.get("rest_total_hits_as_int").map(|v| v == "true").unwrap_or(false) {
        json!(out.total)
    } else {
        json!({"value": out.total, "relation": "eq"})
    };
    let max_score = out.max_score.map(|s| json!(s)).unwrap_or(Value::Null);
    Some(json!({"hits": {"total": total, "max_score": max_score, "hits": out.hits}}))
}

/// Assemble the `hits` envelope, honouring track_total_hits and the
/// `rest_total_hits_as_int` compatibility switch.
pub(crate) fn envelope(out: Outcome, body: &Value, p: &Params) -> Value {
    let out_shards = out.shards;
    let out_failures = out.failures.clone();
    let out_skipped = out.skipped;
    let out_took = out.took_ms;
    let brs = p
        .get("batched_reduce_size")
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| body.get("batched_reduce_size").and_then(|v| v.as_u64()))
        .unwrap_or(512);
    let num_reduce_phases =
        if brs > 1 && out_shards > 1 { out_shards.saturating_sub(1).div_ceil(brs - 1) } else { 1 };
    let track = body.get("track_total_hits").cloned().or_else(|| {
        p.get("track_total_hits").map(|v| match v.as_str() {
            "true" => json!(true),
            "false" => json!(false),
            other => other.parse::<u64>().map(|n| json!(n)).unwrap_or(json!(true)),
        })
    });

    let mut hits_obj = json!({
        "max_score": out.max_score.map(|s| json!(s)).unwrap_or(Value::Null),
        "hits": out.hits,
    });

    let as_int = p.get("rest_total_hits_as_int").map(|v| v == "true").unwrap_or(false);
    let disabled = matches!(track, Some(Value::Bool(false)))
        || matches!(&track, Some(Value::String(s)) if s == "false");
    if disabled {
        // the int form reports -1 for "not tracked"; the object form is omitted
        if as_int {
            hits_obj["total"] = json!(-1);
        }
    } else {
        let limit = match &track {
            Some(Value::Bool(true)) => u64::MAX,
            Some(Value::Number(n)) => n.as_u64().unwrap_or(DEFAULT_TRACK_TOTAL_HITS),
            _ => DEFAULT_TRACK_TOTAL_HITS,
        };
        let (value, relation) = if out.total > limit { (limit, "gte") } else { (out.total, "eq") };
        hits_obj["total"] =
            if as_int { json!(value) } else { json!({"value": value, "relation": relation}) };
    }

    let mut resp = json!({
        "took": out_took,
        "timed_out": false,
        "_shards": {
            "total": out_shards,
            "successful": out_shards.saturating_sub(out_failures.len() as u64),
            "skipped": out_skipped,
            "failed": out_failures.len(),
        },
        "hits": hits_obj,
        "num_reduce_phases": num_reduce_phases,
    });
    if !out_failures.is_empty() {
        resp["_shards"]["failures"] = Value::Array(out_failures);
    }
    if let Some(a) = out.aggs {
        resp["aggregations"] = a;
    }
    if let Some(pr) = out.profile {
        resp["profile"] = pr;
    }
    if let Some(sg) = out.suggest {
        resp["suggest"] = sg;
    }
    resp
}
