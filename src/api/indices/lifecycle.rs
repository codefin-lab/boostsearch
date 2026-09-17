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
    Query(p): Query<Params>,
) -> Response {
    // a forced flush has to be allowed to wait for one already running, or it
    // would have to refuse to do the thing it was asked for
    if p.get("force").map(|v| v != "false").unwrap_or(false)
        && p.get("wait_if_ongoing").map(|v| v == "false").unwrap_or(false)
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
            let started = std::time::Instant::now();
            let _ = g.refresh();
            g.counters.flush.add(started.elapsed().as_nanos() as u64);
        }
    }
    respond(&p, json!({"_shards": tally}))
}

pub async fn refresh_all(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    let names = store.names();
    for n in &names {
        if let Some(st) = store.get(n) {
            let _ = st.write().refresh_external();
        }
    }
    respond(&p, json!({"_shards": shards_over(&store, &names)}))
}

pub async fn refresh_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let targets = store.resolve(&index);
    // a pattern that reaches nothing has nothing to refresh, which is not an
    // error; a name given outright must be there
    if targets.is_empty() && !index.contains('*') && index != "_all" && !index.is_empty() {
        return no_such_index(&index);
    }
    let tally = shards_over(&store, &targets);
    for n in targets {
        if let Some(st) = store.get(&n) {
            let _ = st.write().refresh_external();
        }
    }
    respond(&p, json!({"_shards": tally}))
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
    let expunge = p.get("only_expunge_deletes").map(|v| v != "false").unwrap_or(false);

    // merging is work rather than waiting: on the runtime's own thread it
    // would hold a worker for as long as the merge takes, and the requests
    // that worker was serving with it
    let merging = store.clone();
    let _ = tokio::task::spawn_blocking(move || {
        for name in targets {
            let Some(st) = merging.get(&name) else { continue };
            let mut g = st.write();
            if g.refresh().is_err() {
                continue;
            }
            // each pass leaves fewer segments or fewer deletes; a writer that
            // somehow does neither is not asked forever
            for _pass in 0..64 {
                let metas = g.index.searchable_segment_metas().unwrap_or_default();
                // A segment holding deleted documents is rewritten without
                // them when the merge asks for one segment or for the deletes
                // to go, even when it is already the only one: the reference
                // does, and a segment left alone went on reporting a deleted
                // document that `_stats` had stopped counting.
                let batch: Vec<velocore::index::SegmentId> = if expunge
                    || metas.len() <= max_segments
                {
                    if !(expunge || max_segments == 1) {
                        break;
                    }
                    let with_deletes: Vec<_> =
                        metas.iter().filter(|m| m.num_deleted_docs() > 0).map(|m| m.id()).collect();
                    if with_deletes.is_empty() {
                        break;
                    }
                    // with one segment asked for, everything goes into it
                    if max_segments == 1 && !expunge {
                        metas.iter().map(|m| m.id()).collect()
                    } else {
                        with_deletes
                    }
                } else {
                    // merge the whole set down in one step; VeloCore handles
                    // the rest
                    let take = metas.len() - max_segments + 1;
                    metas.iter().take(take).map(|m| m.id()).collect()
                };
                let searcher = g.reader.searcher();
                let merged_away: Vec<_> = searcher
                    .segment_readers()
                    .iter()
                    .filter(|r| batch.contains(&r.segment_id()))
                    .collect();
                let docs: u64 = merged_away.iter().map(|r| r.max_doc() as u64).sum();
                let before: u64 = merged_away.iter().map(|r| g.segment_bytes(r)).sum();
                drop(merged_away);
                drop(searcher);
                let started = std::time::Instant::now();
                let merged = match g.writer() {
                    Ok(w) => w.merge(&batch).wait().is_ok(),
                    Err(_) => false,
                };
                if !merged {
                    break;
                }
                g.counters.merge.add(started.elapsed().as_nanos() as u64);
                g.counters.merge_docs.fetch_add(docs, std::sync::atomic::Ordering::Relaxed);
                g.counters.merge_bytes.fetch_add(before, std::sync::atomic::Ordering::Relaxed);
                let _ = g.refresh();
                // an expunge rewrites each segment once; asking again would
                // find the rewritten ones clean and stop, but a segment the
                // writer could not clean would be merged forever
                if expunge {
                    break;
                }
            }
        }
    })
    .await;
    respond(
        &p,
        json!({
            "_shards": {"total": touched, "successful": touched, "failed": 0}
        }),
    )
}

/// `_segments` -- what each shard is made of.
///
/// One shard per index here, and VeloCore names its segments by ordinal, so
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
                    "size_in_bytes": g.segment_bytes(reader),
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
                "routing": {"state": "STARTED", "primary": true, "node": "velosearch"},
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
            let mut g = st.write();
            g.closed = true;
            // written down, so the index is still closed after a restart
            g.save_meta();
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
            let mut g = st.write();
            g.closed = false;
            g.save_meta();
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
