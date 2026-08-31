//! An index as a thing: made, opened, closed, resized, rolled over, deleted.

use super::*;

mod resize;
pub use resize::*;
mod shards;
pub use shards::*;

pub async fn create_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(_p): Query<Params>,
    body: String,
) -> Response {
    // An endpoint this server does not answer falls through to here, and a
    // name beginning with an underscore is the API's own, never an index --
    // so a request to `/_reindex` is a request that went unanswered, not a
    // request to create an index called `_reindex`.
    if let Some(r) = reserved_index_name(&index) {
        return r;
    }
    let body: Value = if body.trim().is_empty() {
        json!({})
    } else {
        match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => return err(StatusCode::BAD_REQUEST, "parse_exception", e.to_string()),
        }
    };
    if store.exists(&index) {
        return err(
            StatusCode::BAD_REQUEST,
            "resource_already_exists_exception",
            format!("index [{index}] already exists"),
        );
    }
    // a flat_object holds whatever it is given and is not analysed, so the
    // parameters that describe analysis mean nothing to it
    if let Some(props) = body.pointer("/mappings/properties").and_then(|p| p.as_object()) {
        for (name, def) in props {
            if def.get("type").and_then(|t| t.as_str()) != Some("flat_object") {
                continue;
            }
            let stray: Vec<String> = def
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter(|(k, _)| *k != "type")
                        .map(|(k, v)| {
                            format!(
                                "{k} : {}",
                                v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string())
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !stray.is_empty() {
                return err(
                    StatusCode::BAD_REQUEST,
                    "mapper_parsing_exception",
                    format!(
                        "Mapping definition for [{name}] has unsupported parameters:  [{}]",
                        stray.join(", ")
                    ),
                );
            }
        }
    }
    // an index can only sort itself by a field whose values it can compare,
    // one at a time
    if let Some(fields) = body
        .pointer("/settings/index.sort.field")
        .or_else(|| body.pointer("/settings/index/sort/field"))
    {
        let names: Vec<String> = match fields {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
            _ => Vec::new(),
        };
        for name in names {
            let kind = body
                .pointer(&format!(
                    "/mappings/properties/{}/type",
                    name.replace('.', "/properties/")
                ))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            // a field inside a nested object belongs to the object, not to
            // the document, so the document cannot be sorted by it
            let mut inside_nested = false;
            let mut walked = String::new();
            for part in name.split('.').rev().skip(1).collect::<Vec<_>>().into_iter().rev() {
                walked =
                    if walked.is_empty() { part.to_string() } else { format!("{walked}.{part}") };
                if body
                    .pointer(&format!(
                        "/mappings/properties/{}/type",
                        walked.replace('.', "/properties/")
                    ))
                    .and_then(|t| t.as_str())
                    == Some("nested")
                {
                    inside_nested = true;
                }
            }
            if inside_nested {
                return err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!(
                        "index sorting on nested fields is not supported: found nested sort \
                         field [{name}] in [{index}]"
                    ),
                );
            }
            if matches!(kind, "half_float" | "nested" | "object" | "text" | "") {
                return err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!("docvalues not found for index sort field:[{name}]"),
                );
            }
        }
    }
    // adaptive shard selection only makes sense on an append-only index
    let setting = |k: &str| -> Option<String> {
        body.pointer(&format!("/settings/index/{k}"))
            .or_else(|| body.pointer(&format!("/settings/index.{k}")))
            .map(|v| v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string()))
    };
    if setting("bulk.adaptive_shard_selection.enabled").as_deref() == Some("true")
        && setting("append_only.enabled").as_deref() != Some("true")
    {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "index [{index}] is not append-only index, bulk adaptive shard selection is \
                 enabled, which is not supported. Please disable bulk adaptive shard selection \
                 or set index to append-only index."
            ),
        );
    }
    if body
        .get("mappings")
        .map(|m| {
            m.get("properties")
                .and_then(|p| p.as_object())
                .map(|o| o.keys().any(|k| k.trim().is_empty()))
                .unwrap_or(false)
        })
        .unwrap_or(false)
    {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "field name cannot be an empty string",
        );
    }
    match store.create(&index, &body) {
        Ok(()) => axum::Json(json!({
            "acknowledged": true, "shards_acknowledged": true, "index": index
        }))
        .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string()),
    }
}

pub async fn delete_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let lenient = ignore_unavailable(&p);
    // an alias names indices without being one; deleting it would have to
    // mean deleting what it points at, which is not what was asked
    for part in index.split(',').map(|n| n.trim()).filter(|n| !n.is_empty()) {
        if !part.contains('*') && store.is_alias(part) && !lenient {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!(
                    "The provided expression [{part}] matches an alias, specify the \
                     corresponding concrete indices instead."
                ),
            );
        }
    }
    // a pattern reaches for indices, not for the aliases that stand in front
    // of them, so it is matched against the real names only
    let mut targets: Vec<String> = Vec::new();
    let mut missing: Option<String> = None;
    for part in index.split(',').map(|n| n.trim()).filter(|n| !n.is_empty()) {
        if part.contains('*') {
            for n in store.names() {
                if crate::store::glob_match(part, &n) && !targets.contains(&n) {
                    targets.push(n);
                }
            }
        } else if store.is_alias(part) {
            continue;
        } else {
            let found = store.resolve(part);
            if found.is_empty() {
                missing.get_or_insert_with(|| part.to_string());
            }
            for n in found {
                if !targets.contains(&n) {
                    targets.push(n);
                }
            }
        }
    }
    if let Some(name) = missing.filter(|_| !lenient) {
        return no_such_index(&name);
    }
    // `allow_no_indices=false` makes an expression that reaches nothing an
    // error rather than a request with nothing to do
    // `allow_no_indices=false` is asked of each pattern in turn: an
    // expression is only satisfied if every part of it reached something
    let allow_none = p.get("allow_no_indices").map(|v| v != "false").unwrap_or(true);
    if !allow_none {
        for part in index.split(',').map(|n| n.trim()).filter(|n| n.contains('*')) {
            let reached = store.names().iter().any(|n| crate::store::glob_match(part, n));
            if !reached {
                return no_such_index(part);
            }
        }
        if targets.is_empty() {
            return no_such_index(&index);
        }
    }
    for n in &targets {
        store.delete(n);
    }
    axum::Json(json!({"acknowledged": true})).into_response()
}

pub async fn index_exists(State(store): State<Store>, Path(index): Path<String>) -> Response {
    if store.resolve(&index).is_empty() {
        StatusCode::NOT_FOUND.into_response()
    } else {
        StatusCode::OK.into_response()
    }
}

/// `_flush` writes what is buffered and makes it searchable.
///
/// The distinction OpenSearch draws is between committing to disk and making
/// documents visible; here committing does both, so a flush is a refresh that
/// also settles the writer.
pub async fn flush(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(_p): Query<Params>,
) -> Response {
    // a forced flush has to be allowed to wait for one already running, or it
    // would have to refuse to do the thing it was asked for
    if _p.get("force").map(|v| v != "false").unwrap_or(false)
        && _p.get("wait_if_ongoing").map(|v| v == "false").unwrap_or(false)
    {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: wait_if_ongoing must be true for a force flush;",
        );
    }
    let targets = match index {
        Some(Path(i)) => {
            let t = store.resolve(&i);
            if t.is_empty() {
                return no_such_index(&i);
            }
            t
        }
        None => store.names(),
    };
    let tally = shards_over(&store, &targets);
    for n in targets {
        if let Some(st) = store.get(&n) {
            let mut g = st.write();
            let _ = g.refresh();
            g.flushes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    axum::Json(json!({"_shards": tally})).into_response()
}

pub async fn refresh_all(State(store): State<Store>) -> Response {
    let names = store.names();
    for n in &names {
        if let Some(st) = store.get(n) {
            let _ = st.write().refresh();
        }
    }
    axum::Json(json!({"_shards": shards_over(&store, &names)})).into_response()
}

pub async fn refresh_index(State(store): State<Store>, Path(index): Path<String>) -> Response {
    let targets = store.resolve(&index);
    // a pattern that reaches nothing has nothing to refresh, which is not an
    // error; a name given outright must be there
    if targets.is_empty() && !index.contains('*') && index != "_all" && !index.is_empty() {
        return no_such_index(&index);
    }
    let tally = shards_over(&store, &targets);
    for n in targets {
        if let Some(st) = store.get(&n) {
            let _ = st.write().refresh();
        }
    }
    axum::Json(json!({"_shards": tally})).into_response()
}

/// Defaults a type carries even when the request did not spell them out.
pub(crate) fn add_type_defaults(node: &mut Value) {
    let Some(obj) = node.as_object_mut() else { return };
    if obj.get("type").and_then(|t| t.as_str()) == Some("wildcard")
        && !obj.contains_key("doc_values")
    {
        obj.insert("doc_values".into(), json!(true));
    }
    for key in ["properties", "fields"] {
        if let Some(children) = obj.get_mut(key).and_then(|c| c.as_object_mut()) {
            for (_, child) in children.iter_mut() {
                add_type_defaults(child);
            }
        }
    }
}

/// `_forcemerge` collapses segments. Fewer segments means less per-segment setup
/// on every search, which matters most for aggregations: each one opens columns
/// and builds its own intermediate result per segment before they are merged.
pub async fn force_merge(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_else(|| "_all".into());
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" {
        return no_such_index(&expr);
    }
    // a merge reaches every copy of a shard unless told to keep to the
    // primaries, and a replica is a copy this node does not hold
    let primary_only = p.get("primary_only").map(|v| v != "false").unwrap_or(false);
    let touched: u64 = targets
        .iter()
        .filter_map(|n| store.get(n))
        .map(|st| {
            let g = st.read();
            let shards = g.numeric_setting("number_of_shards").unwrap_or(1).max(1);
            let copies = if primary_only {
                1
            } else {
                1 + g.numeric_setting("number_of_replicas").unwrap_or(0)
            };
            shards * copies
        })
        .sum();
    let max_segments: usize =
        p.get("max_num_segments").and_then(|v| v.parse().ok()).unwrap_or(1).max(1);

    for name in targets {
        let Some(st) = store.get(&name) else { continue };
        let mut g = st.write();
        if g.refresh().is_err() {
            continue;
        }
        loop {
            let ids: Vec<boostcore::index::SegmentId> = g
                .index
                .searchable_segment_metas()
                .unwrap_or_default()
                .iter()
                .map(|m| m.id())
                .collect();
            if ids.len() <= max_segments {
                break;
            }
            // merge the whole set down in one step; BoostCore handles the rest
            let take = ids.len() - max_segments + 1;
            let batch: Vec<_> = ids.into_iter().take(take).collect();
            let merged = match g.writer() {
                Ok(w) => w.merge(&batch).wait().is_ok(),
                Err(_) => false,
            };
            if !merged {
                break;
            }
            let _ = g.refresh();
        }
    }
    respond(
        &p,
        json!({
            "_shards": {"total": touched, "successful": touched, "failed": 0}
        }),
    )
}

/// `_segments` -- what each shard is made of.
///
/// One shard per index here, and BoostCore names its segments by ordinal, so
/// they are reported as `_0`, `_1` and so on to match the shape the API has.
pub async fn segments(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let targets = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
    let allow_none = p.get("allow_no_indices").map(|v| v != "false").unwrap_or(true);
    if targets.is_empty() {
        if !allow_none || (!expr.is_empty() && !expr.contains('*') && !store.exists(&expr)) {
            return no_such_index(&expr);
        }
        return respond(
            &p,
            json!({
                "_shards": {"total": 0, "successful": 0, "failed": 0},
                "indices": {},
            }),
        );
    }
    let mut indices = serde_json::Map::new();
    let mut total = 0u64;
    for n in &targets {
        let Some(st) = store.get(n) else { continue };
        let g = st.read();
        if g.closed {
            // a closed index has nothing to report; the caller decides whether
            // that is an error or simply nothing
            if p.get("ignore_unavailable").map(|v| v != "false").unwrap_or(false) {
                continue;
            }
            return err(
                StatusCode::BAD_REQUEST,
                "index_closed_exception",
                format!("closed index [{n}]"),
            );
        }
        let searcher = g.reader.searcher();
        let mut segs = serde_json::Map::new();
        for (i, reader) in searcher.segment_readers().iter().enumerate() {
            segs.insert(
                format!("_{i}"),
                json!({
                    "generation": i,
                    "num_docs": reader.num_docs(),
                    "deleted_docs": reader.num_deleted_docs(),
                    "size_in_bytes": 0,
                    "memory_in_bytes": 0,
                    "committed": true,
                    "search": true,
                    "version": "9.0.0",
                    "compound": true,
                    "attributes": {},
                }),
            );
        }
        total += 1;
        indices.insert(
            n.clone(),
            json!({"shards": {"0": [{
                "routing": {"state": "STARTED", "primary": true, "node": "boostsearch"},
                "num_committed_segments": segs.len(),
                "num_search_segments": segs.len(),
                "segments": Value::Object(segs),
            }]}}),
        );
    }
    respond(
        &p,
        json!({
            "_shards": {"total": total, "successful": total, "failed": 0},
            "indices": Value::Object(indices),
        }),
    )
}

/// Names beginning with an underscore are reserved for the API's own
/// endpoints, so one cannot also be an index.
pub(crate) fn reserved_index_name(expr: &str) -> Option<Response> {
    for part in expr.split(',').map(|n| n.trim()) {
        if part.starts_with('_') && !matches!(part, "_all" | "_any") {
            return Some(err(
                StatusCode::BAD_REQUEST,
                "invalid_index_name_exception",
                format!("Invalid index name [{part}], must not start with '_'."),
            ));
        }
    }
    None
}

pub async fn get_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if let Some(r) = reserved_index_name(&index) {
        return r;
    }
    // `expand_wildcards` says which states a pattern reaches
    // health looks at every index by default, closed ones included: a closed
    // index still has shards, and they still count
    let states = p.get("expand_wildcards").map(|v| v.as_str()).unwrap_or("all");
    let want_open = states.split(',').any(|w| matches!(w.trim(), "open" | "all"));
    let want_closed = states.split(',').any(|w| matches!(w.trim(), "closed" | "all"));
    let targets = store.resolve(&index);
    if targets.is_empty() && !index.contains('*') && index != "_all" && !ignore_unavailable(&p) {
        return no_such_index(&index);
    }
    // a pattern reaching nothing is an error only when the caller said so
    let allow_none = p.get("allow_no_indices").map(|v| v != "false").unwrap_or(true);
    if targets.is_empty() && !allow_none {
        return no_such_index(&index);
    }
    let mut out = serde_json::Map::new();
    for n in targets {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        // a pattern only reaches the states it was told to; a name given
        // outright reaches its index whatever state it is in
        if index.contains('*') && ((g.closed && !want_closed) || (!g.closed && !want_open)) {
            continue;
        }
        // a pattern only reaches the states it was told to; a name given
        // outright reaches its index whatever state it is in
        if index.contains('*') && ((g.closed && !want_closed) || (!g.closed && !want_open)) {
            continue;
        }
        let mut aliases = serde_json::Map::new();
        for (a, def) in &g.aliases {
            aliases.insert(a.clone(), def.clone());
        }
        let mut settings = g.effective_settings();
        if flag(&p, "human") {
            add_human_settings(&mut settings, &g);
        }
        out.insert(
            n.clone(),
            json!({
                "aliases": Value::Object(aliases),
                "mappings": if g.mapping.raw.is_null() { json!({}) } else { g.mapping.raw.clone() },
                "settings": settings,
            }),
        );
    }
    respond(&p, Value::Object(out))
}

pub async fn close_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let targets = store.resolve(&index);
    if targets.is_empty() && !index.contains('*') {
        return no_such_index(&index);
    }
    let mut per = serde_json::Map::new();
    for n in targets {
        if let Some(st) = store.get(&n) {
            st.write().closed = true;
            per.insert(n.clone(), json!({"closed": true}));
        }
    }
    respond(&p, json!({"acknowledged": true, "shards_acknowledged": true, "indices": per}))
}

pub async fn open_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let targets = store.resolve(&index);
    if targets.is_empty() && !index.contains('*') {
        return no_such_index(&index);
    }
    for n in &targets {
        if let Some(st) = store.get(n) {
            st.write().closed = false;
        }
    }
    // `wait_for_completion=false` asks for the work to be tracked rather than
    // waited on; the index is already open, so the task is a finished one
    if p.get("wait_for_completion").map(|v| v == "false").unwrap_or(false) {
        return respond(&p, json!({"task": format!("node-0:open indices [{index}]")}));
    }
    respond(&p, json!({"acknowledged": true, "shards_acknowledged": true}))
}
