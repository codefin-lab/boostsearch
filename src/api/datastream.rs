//! Data streams: a name that writes to whichever index is current.

use super::*;

/// The composable template a data stream would be made from: the one whose
/// patterns the name fits, that says it backs a data stream, and that outranks
/// the others claiming the same name.
pub(crate) fn data_stream_template(store: &Store, name: &str) -> Option<(String, Value)> {
    let mut best: Option<(i64, String, Value)> = None;
    for (tname, t) in store.get_templates() {
        let Some(body) = t.get("__composable").cloned() else { continue };
        if body.get("data_stream").is_none() {
            continue;
        }
        let matches = body
            .get("index_patterns")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .any(|pat| pat == name || crate::store::glob_match(pat, name))
            })
            .unwrap_or(false);
        if !matches {
            continue;
        }
        let priority = body.get("priority").and_then(|v| v.as_i64()).unwrap_or(0);
        if best.as_ref().map(|(p, _, _)| priority > *p).unwrap_or(true) {
            best = Some((priority, tname, body));
        }
    }
    best.map(|(_, n, b)| (n, b))
}

/// The index a data stream's documents are actually written to.
pub(crate) fn backing_index(name: &str, generation: u64) -> String {
    format!(".ds-{name}-{generation:06}")
}

/// `PUT /_data_stream/{name}` -- a stream is an index that rolls over on its
/// own, so making one means making the index behind it.
pub async fn create_data_stream(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if store.data_streams().contains_key(&name) {
        return err(
            StatusCode::BAD_REQUEST,
            "resource_already_exists_exception",
            format!("data_stream [{name}] already exists"),
        );
    }
    let Some((template, _)) = data_stream_template(&store, &name) else {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("no matching index template found for data stream [{name}]"),
        );
    };
    if let Err(e) = store.create(&backing_index(&name, 1), &json!({})) {
        return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string());
    }
    store.add_data_stream(&name, &template);
    respond(&p, json!({"acknowledged": true}))
}

/// A write to a name nothing holds, that a data stream template matches,
/// makes the stream: its first backing index, and the stream in front of it.
/// It made an ordinary index of the stream's name instead, and the stream
/// the template described never existed. Whether anything was made.
pub(crate) fn create_stream_for_write(store: &Store, name: &str) -> bool {
    if store.exists(name) || store.is_alias(name) || store.data_streams().contains_key(name) {
        return false;
    }
    let Some((template, _)) = data_stream_template(store, name) else { return false };
    if store.create(&backing_index(name, 1), &json!({})).is_err() {
        return false;
    }
    store.add_data_stream(name, &template);
    true
}

/// What a document written into a data stream lacks: the timestamp field,
/// single-valued. The reference refuses it as a document it cannot parse.
pub(crate) fn stream_document_refusal(
    store: &Store,
    index: &str,
    source: &Value,
) -> Option<Response> {
    let stream = store.stream_behind(index)?;
    let template = store.data_streams().get(&stream).cloned()?;
    // read from the template alone: a bulk asks this holding the backing
    // index's lock, and the stream's entry reads every backing index
    let field = timestamp_field(store, &template);
    let value =
        source.pointer(&format!("/{}", field.replace('.', "/"))).or_else(|| source.get(&field));
    let single = matches!(value, Some(Value::String(_)) | Some(Value::Number(_)));
    if single {
        return None;
    }
    Some(crate::api::shared::parse_refusal_caused_by(
        "illegal_argument_exception",
        &format!("documents must contain a single-valued timestamp field '{field}' of date type"),
    ))
}

/// The field a stream's template says carries its time.
fn timestamp_field(store: &Store, template: &str) -> String {
    store
        .get_templates()
        .get(template)
        .and_then(|t| {
            // the composable form the template was written in is kept beside
            // the flattened one the index creation reads
            t.pointer("/__composable/data_stream/timestamp_field/name")
                .or_else(|| t.pointer("/__composable/data_stream/timestamp_field"))
                .or_else(|| t.pointer("/data_stream/timestamp_field/name"))
                .or_else(|| t.pointer("/data_stream/timestamp_field"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "@timestamp".to_string())
}

pub(crate) fn data_stream_entry(store: &Store, name: &str, template: &str) -> Value {
    let field = timestamp_field(store, template);
    // every backing index the stream has, oldest first, and the generation is
    // the newest of them: it used to say one index and generation 1 whatever
    // had happened, so a stream that had rolled over reported the index it
    // had rolled out of and nothing else
    let held = store.backing_indices(name);
    let held = if held.is_empty() { vec![backing_index(name, 1)] } else { held };
    let generation = held
        .last()
        .and_then(|n| n.rsplit('-').next())
        .and_then(|g| g.parse::<u64>().ok())
        .unwrap_or(1);
    json!({
        "name": name,
        "timestamp_field": {"name": field},
        "indices": held
            .iter()
            .map(|n| {
                // the index's own uuid: one made up from the name was the same
                // for every stream ever made under it, and matched nothing
                let uuid = store
                    .get(n)
                    .map(|st| st.read().uuid.clone())
                    .unwrap_or_else(|| crate::store::index_uuid(n));
                json!({"index_name": n, "index_uuid": uuid})
            })
            .collect::<Vec<_>>(),
        "generation": generation,
        "status": "GREEN",
        "template": template,
    })
}

/// `GET /_data_stream` and `GET /_data_stream/{name}`.
pub async fn get_data_stream(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let want = name.map(|Path(n)| n).unwrap_or_else(|| "*".into());
    let mut out: Vec<Value> = store
        .data_streams()
        .into_iter()
        .filter(|(n, _)| {
            want.split(',').any(|pat| {
                let pat = pat.trim();
                pat == "*" || pat == "_all" || pat == n || crate::store::glob_match(pat, n)
            })
        })
        .map(|(n, t)| data_stream_entry(&store, &n, &t))
        .collect();
    if out.is_empty() && !want.contains('*') && want != "_all" {
        // the whole shape OpenSearch gives, resource and all
        return crate::api::shared::no_such_index(&want);
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    respond(&p, json!({"data_streams": out}))
}

/// `DELETE /_data_stream/{name}`.
pub async fn delete_data_stream(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    // Every index behind the stream goes with it. Only the first generation
    // was deleted: after a rollover the write index stayed, documents and
    // all, and a stream made again under the same name adopted it -- the
    // documents of a stream that had been deleted came back. The indices are
    // read before the stream is forgotten, since forgetting it is what
    // stops them being findable as its own.
    let mut backing: Vec<String> = Vec::new();
    for (n, t) in store.data_streams() {
        let named = name.split(',').any(|pat| {
            let pat = pat.trim();
            pat == "*" || pat == "_all" || pat == n || crate::store::glob_match(pat, &n)
        });
        if !named {
            continue;
        }
        let entry = data_stream_entry(&store, &n, &t);
        if let Some(list) = entry["indices"].as_array() {
            backing.extend(
                list.iter()
                    .filter_map(|i| i.get("index_name").and_then(|v| v.as_str()))
                    .map(String::from),
            );
        }
    }
    let gone = store.remove_data_stream(&name);
    for index in &backing {
        store.delete(index);
    }
    for g in &gone {
        store.delete(&backing_index(g, 1));
    }
    // a stream that is not there has been deleted already: the reference
    // acknowledges it, and a cleanup that runs twice is not an error
    respond(&p, json!({"acknowledged": true}))
}

/// `GET /_data_stream/_stats` and `GET /_data_stream/{name}/_stats`.
///
/// What each stream holds: how many indices are behind it, how much they take
/// on disk, and the latest instant any of their documents carries.
pub async fn data_stream_stats(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let want = name.map(|Path(n)| n).unwrap_or_else(|| "*".into());
    let mut streams: Vec<Value> = Vec::new();
    let mut backing = 0usize;
    let mut total_bytes = 0u64;
    let human = p.get("human").map(|v| v != "false").unwrap_or(false);
    for (n, t) in store.data_streams() {
        let _ = &t;
        let named = want.split(',').any(|pat| {
            let pat = pat.trim();
            pat == "*" || pat == "_all" || pat == n || crate::store::glob_match(pat, &n)
        });
        if !named {
            continue;
        }
        let entry = data_stream_entry(&store, &n, &t);
        let indices: Vec<String> = entry["indices"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|i| i.get("index_name").and_then(|v| v.as_str()))
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        let mut bytes = 0u64;
        // the newest instant the stream's own time field carries
        let field = "@timestamp";
        let mut latest = 0i64;
        for idx in &indices {
            bytes += store.index_size(idx);
            let probe = json!({
                "size": 0,
                "aggs": {"newest": {"max": {"field": field}}},
            });
            if let Ok(found) = crate::search::run(&store, idx, &probe, &Params::new())
                && let Some(v) = found
                    .aggs
                    .as_ref()
                    .and_then(|a| a.pointer("/newest/value"))
                    .and_then(|v| v.as_f64())
            {
                latest = latest.max(v as i64);
            }
        }
        backing += indices.len();
        total_bytes += bytes;
        let mut one = json!({
            "data_stream": n,
            "backing_indices": indices.len(),
            "store_size_bytes": bytes,
            "maximum_timestamp": latest,
        });
        // the readable size is for a person, who asks for it with `human`
        if human {
            one["store_size"] = json!(crate::api::shared::readable_bytes(bytes));
        }
        streams.push(one);
    }
    streams.sort_by(|a, b| a["data_stream"].as_str().cmp(&b["data_stream"].as_str()));
    let mut out = json!({
        "_shards": {"total": backing, "successful": backing, "failed": 0},
        "data_stream_count": streams.len(),
        "backing_indices": backing,
        "total_store_size_bytes": total_bytes,
        "data_streams": streams,
    });
    if human {
        out["total_store_size"] = json!(crate::api::shared::readable_bytes(total_bytes));
    }
    respond(&p, out)
}
