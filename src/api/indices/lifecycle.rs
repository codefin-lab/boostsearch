//! Opening, closing, refreshing and merging what an index already holds.

use super::*;

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
        // the task is named after the node that ran it, as OpenSearch names it
        let me = crate::cluster::identity().id.as_str().to_string();
        return respond(&p, json!({"task": format!("{me}:open indices [{index}]")}));
    }
    respond(&p, json!({"acknowledged": true, "shards_acknowledged": true}))
}
