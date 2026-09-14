//! Counters: per index, per metric, summed the way OpenSearch sums them.

use super::*;

/// What a scroll refuses before it starts.
/// Which fields a search reads ordinals for: sorting on one loads it, and so
/// does a terms aggregation, unless it was asked to build its buckets in a map
/// instead.
pub(crate) fn fielddata_fields_of(body: &Value, out: &mut Vec<String>) {
    match body {
        Value::Object(o) => {
            if let Some(t) = o.get("terms").and_then(|t| t.as_object()) {
                let mapped = t
                    .get("execution_hint")
                    .and_then(|h| h.as_str())
                    .map(|h| h == "map")
                    .unwrap_or(false);
                if !mapped && let Some(f) = t.get("field").and_then(|f| f.as_str()) {
                    out.push(f.to_string());
                }
            }
            for (k, v) in o {
                if k == "sort" {
                    match v {
                        Value::String(f) => out.push(f.clone()),
                        Value::Array(a) => {
                            for item in a {
                                match item {
                                    Value::String(f) => out.push(f.clone()),
                                    Value::Object(f) => {
                                        out.extend(f.keys().cloned());
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Value::Object(f) => out.extend(f.keys().cloned()),
                        _ => {}
                    }
                    continue;
                }
                fielddata_fields_of(v, out);
            }
        }
        Value::Array(a) => {
            for v in a {
                fielddata_fields_of(v, out);
            }
        }
        _ => {}
    }
}

/// Note what this search loaded, so the fielddata statistic can report it.
pub(crate) fn note_fielddata(store: &Store, expr: &str, body: &Value) {
    // nothing is loaded by a search that neither sorts nor aggregates
    if body.get("sort").is_none()
        && body.get("aggs").is_none()
        && body.get("aggregations").is_none()
    {
        return;
    }
    let mut fields = Vec::new();
    fielddata_fields_of(body, &mut fields);
    fields.retain(|f| !f.starts_with('_'));
    if fields.is_empty() {
        return;
    }
    for n in store.resolve(expr) {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        // reading is cheap and shared; the write lock is only worth taking
        // for a field that has not been loaded before
        if g.loaded_fielddata.read().is_superset(&fields.iter().cloned().collect()) {
            continue;
        }
        let mut loaded = g.loaded_fielddata.write();
        for f in &fields {
            loaded.insert(f.clone());
        }
    }
}

/// Which fields a `fields=`-style parameter names.
///
/// Absent means the caller wants no per-field breakdown at all, which is not
/// the same as naming none.
pub(crate) fn stats_field_patterns(p: &Params, specific: &str) -> Option<Vec<String>> {
    for key in [specific, "fields"] {
        if let Some(v) = p.get(key) {
            return Some(
                v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            );
        }
    }
    None
}

pub(crate) fn stats_field_wanted(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|pat| {
        pat == "*" || pat == "_all" || pat == name || crate::store::glob_match(pat, name)
    })
}

pub(crate) fn index_stats(
    st: &IdxState,
    on_disk: u64,
    want_groups: Option<&[String]>,
    p: &Params,
    cache: Option<&crate::search::RequestCache>,
) -> Value {
    let searcher = st.reader.searcher();
    let docs = searcher.num_docs();
    // only a field whose ordinals were actually read counts as fielddata
    let loaded = st.loaded_fielddata.read().clone();
    let cols: std::collections::HashMap<String, u64> =
        st.field_column_bytes().into_iter().filter(|(k, _)| loaded.contains(k)).collect();
    let fielddata_total: u64 = cols.values().sum();
    // a per-field breakdown is reported only where the request asked for one,
    // and a field appears under the statistic its type can carry: fielddata
    // for a text field, completion for a completion field
    let is_completion = |name: &str| st.mapping.type_of(name) == Some("completion");
    let fielddata_fields: Value = match stats_field_patterns(p, "fielddata_fields") {
        None => Value::Null,
        Some(pats) => Value::Object(
            cols.iter()
                .filter(|(k, _)| !is_completion(k) && stats_field_wanted(&pats, k))
                .map(|(k, v)| (k.clone(), json!({"memory_size_in_bytes": v})))
                .collect(),
        ),
    };
    let completion_names: Vec<String> = st
        .mapping
        .types
        .iter()
        .filter(|(_, t)| t.as_str() == Some("completion"))
        .map(|(k, _)| k.clone())
        .collect();
    let completion_total: u64 = completion_names.len() as u64 * 64 * docs.max(1);
    let completion_fields: Value = match stats_field_patterns(p, "completion_fields") {
        None => Value::Null,
        Some(pats) => Value::Object(
            completion_names
                .iter()
                .filter(|k| stats_field_wanted(&pats, k))
                .map(|k| (k.clone(), json!({"size_in_bytes": 64 * docs.max(1)})))
                .collect(),
        ),
    };
    // nothing is held in a fielddata cache here: term lookups read the
    // column directly, so there is no memory to report against it
    let mut fielddata_stat = json!({"memory_size_in_bytes": fielddata_total, "evictions": 0});
    if let Value::Object(f) = fielddata_fields {
        fielddata_stat["fields"] = Value::Object(f);
    }
    let mut completion_stat = json!({"size_in_bytes": completion_total});
    if let Value::Object(f) = completion_fields {
        completion_stat["fields"] = Value::Object(f);
    }

    let c = &st.counters;
    let load = |a: &std::sync::atomic::AtomicU64| a.load(std::sync::atomic::Ordering::Relaxed);
    // one set of search counters, for the index or for one group of it
    let search_section = |t: &crate::store::counters::SearchTally| {
        json!({
            "query_total": t.query.total(), "query_time_in_millis": t.query.millis(),
            "query_current": t.query.current(), "query_failed": load(&t.query_failed),
            "fetch_total": t.fetch.total(), "fetch_time_in_millis": t.fetch.millis(),
            "fetch_current": t.fetch.current(),
            "scroll_total": t.scroll.total(), "scroll_time_in_millis": t.scroll.millis(),
            "scroll_current": t.scroll.current(),
            "suggest_total": t.suggest.total(), "suggest_time_in_millis": t.suggest.millis(),
            "suggest_current": t.suggest.current(),
            "point_in_time_total": 0, "point_in_time_time_in_millis": 0,
            "point_in_time_current": 0,
            // a search that reads its segments side by side counts what
            // that cost separately
            "concurrent_query_total": 0, "concurrent_query_time_in_millis": 0,
            "concurrent_query_current": 0, "concurrent_avg_slice_count": 0.0,
            "search_idle_reactivate_count_total": 0,
            "startree_query_total": 0, "startree_query_time_in_millis": 0,
            "startree_query_current": 0, "startree_query_failed": 0,
        })
    };
    // `groups` is only reported for the groups the request named
    let groups_field = want_groups.map(|w| {
        Value::Object(
            c.groups
                .read()
                .iter()
                // the request may name groups outright, or by pattern
                .filter(|(k, _)| {
                    w.iter().any(|g| g == "_all" || g == *k || crate::store::glob_match(g, k))
                })
                .map(|(k, t)| (k.clone(), search_section(t)))
                .collect(),
        )
    });
    // a document deleted or replaced stays in its segment, counted as
    // deleted, until a merge rewrites the segment -- as `_cat/segments`
    // reports it segment by segment
    let deleted: u64 = searcher.segment_readers().iter().map(|r| r.num_deleted_docs() as u64).sum();
    // `human` asks for the readable form beside the machine one
    let human = p.get("human").map(|v| v != "false").unwrap_or(false);
    let mut search = search_section(&c.search);
    search["open_contexts"] = json!(0);
    let mut out = json!({
        "docs": {"count": docs, "deleted": if st.closed { 0 } else { deleted }},
        "store": {"size_in_bytes": on_disk, "reserved_in_bytes": 0},
        "indexing": {"index_total": c.index.total(), "index_time_in_millis": c.index.millis(),
                     "index_current": c.index.current(),
                     "index_failed": load(&c.index_failed),
                     "delete_total": c.delete.total(),
                     "delete_time_in_millis": c.delete.millis(),
                     "delete_current": c.delete.current(),
                     "noop_update_total":
                         st.noop_updates.load(std::sync::atomic::Ordering::Relaxed),
                     "is_throttled": false,
                     "throttle_time_in_millis": 0,
                     "max_last_index_request_timestamp": load(&c.last_index_ms),
                     // what each write answered with, counted by status
                     "doc_status": {}},
        "get": {"total": c.get.total(),
                "getTime": crate::api::shared::time_value_text(c.get.millis() * 1_000_000),
                "time_in_millis": c.get.millis(),
                "exists_total": c.get_exists.total(),
                "exists_time_in_millis": c.get_exists.millis(),
                "missing_total": c.get_missing.total(),
                "missing_time_in_millis": c.get_missing.millis(), "current": c.get.current()},
        "search": search,
        "merges": {"current": 0, "current_docs": 0, "current_size_in_bytes": 0,
                   "total": c.merge.total(), "total_time_in_millis": c.merge.millis(),
                   "total_docs": load(&c.merge_docs),
                   "total_size_in_bytes": load(&c.merge_bytes),
                   "total_stopped_time_in_millis": 0, "total_throttled_time_in_millis": 0,
                   "total_auto_throttle_in_bytes": 20_971_520_i64,
                   "unreferenced_file_cleanups_performed": 0,
                   "warmer": {"ongoing_count": 0, "total_bytes_received": 0,
                              "total_bytes_sent": 0, "total_failure_count": 0,
                              "total_invocations_count": 0, "total_receive_time_millis": 0,
                              "total_send_time_millis": 0, "total_time_millis": 0}},
        "refresh": {"total": c.refresh.total(), "total_time_in_millis": c.refresh.millis(),
                    "external_total": c.refresh_external.total(),
                    "external_total_time_in_millis": c.refresh_external.millis(),
                    "listeners": 0},
        "flush": {"total": c.flush.total(), "periodic": 0,
                  "total_time_in_millis": c.flush.millis()},
        "warmer": {"current": 0, "total": 0, "total_time_in_millis": 0},
        "query_cache": {"memory_size_in_bytes": 0, "total_count": 0, "hit_count": 0,
                        "miss_count": 0, "cache_size": 0, "cache_count": 0, "evictions": 0},
        "fielddata": fielddata_stat,
        "completion": completion_stat,
        // a closed index has nothing loaded, so it reports no segments unless
        // the caller asks for the ones sitting unloaded on disk
        "segments": {"count": if st.closed
                        && !p.get("include_unloaded_segments").map(|v| v == "true").unwrap_or(false)
                    { 0 } else { searcher.segment_readers().len() },
                     "memory_in_bytes": 0,
                     "terms_memory_in_bytes": 0, "stored_fields_memory_in_bytes": 0,
                     "term_vectors_memory_in_bytes": 0, "norms_memory_in_bytes": 0,
                     "points_memory_in_bytes": 0, "doc_values_memory_in_bytes": 0,
                     "index_writer_memory_in_bytes": 0, "version_map_memory_in_bytes": 0,
                     "fixed_bit_set_memory_in_bytes": 0, "max_unsafe_auto_id_timestamp": -1,
                     "file_sizes": {}},
        // what the translog holds is what a crash would have to replay, which
        // is the file on disk where there is one
        "translog": {"operations": if st.closed { 0 } else { st.pending.len() },
                     "size_in_bytes": st.translog_bytes().max(st.pending_bytes as u64).max(55),
                     "uncommitted_operations":
                        if st.closed { 0 } else { st.pending.len() },
                     "uncommitted_size_in_bytes":
                        st.translog_bytes().max(st.pending_bytes as u64).max(55),
                     "earliest_last_modified_age": 0,
                     "remote_store": {"upload": {"total_uploads": {"started": 0, "failed": 0, "succeeded": 0}}}},
        "request_cache": {
            "memory_size_in_bytes": cache.map(|c| c.bytes()).unwrap_or(0),
            "evictions": cache
                .map(|c| c.evictions.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(0),
            "hit_count": st.request_cache_hit.load(std::sync::atomic::Ordering::Relaxed),
            "miss_count": st.request_cache_miss.load(std::sync::atomic::Ordering::Relaxed)
        },
        "recovery": {"current_as_source": 0, "current_as_target": 0, "throttle_time_in_millis": 0},
    });
    // what a remote store and a segment replication would have moved, which
    // for an index that has neither is nothing at all
    out["segments"]["remote_store"] = json!({
        "upload": {
            "total_upload_size": {"started_bytes": 0, "succeeded_bytes": 0, "failed_bytes": 0},
            "refresh_size_lag": {"total_bytes": 0, "max_bytes": 0},
            "max_refresh_time_lag_in_millis": 0,
            "total_time_spent_in_millis": 0,
            "pressure": {"total_rejections": 0},
        },
        "download": {
            "total_download_size": {"started_bytes": 0, "succeeded_bytes": 0, "failed_bytes": 0},
            "total_time_spent_in_millis": 0,
        },
    });
    out["segments"]["segment_replication"] = json!({
        "max_bytes_behind": 0, "total_bytes_behind": 0, "max_replication_lag": 0,
    });
    out["translog"]["remote_store"] = json!({"upload": {
        "total_uploads": {"started": 0, "failed": 0, "succeeded": 0},
        "total_upload_size": {"started_bytes": 0, "failed_bytes": 0, "succeeded_bytes": 0},
    }});

    if let Some(groups) = groups_field {
        out["search"]["groups"] = groups;
    }
    if human {
        // the readable form of what a get cost, under both of the names
        // OpenSearch writes it as
        let text = |ms: u64| json!(crate::api::shared::time_value_text(ms * 1_000_000));
        out["get"]["time"] = text(c.get.millis());
        out["get"]["getTime"] = text(c.get.millis());
        out["get"]["exists_time"] = text(c.get_exists.millis());
        out["get"]["missing_time"] = text(c.get_missing.millis());
    }
    out
}

pub(crate) fn sum_stats(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let mut out = x.clone();
            for (k, v) in y {
                let merged = match x.get(k) {
                    Some(prev) => sum_stats(prev, v),
                    None => v.clone(),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        // a count stays a whole number when added up: summed as floats,
        // `query_total` came back as `3.0`, which a client reading a long
        // refuses
        (Value::Number(x), Value::Number(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(a), Some(b)) => json!(a.saturating_add(b)),
            _ => json!(x.as_f64().unwrap_or(0.0) + y.as_f64().unwrap_or(0.0)),
        },
        _ => b.clone(),
    }
}

pub async fn stats_metric(
    State(store): State<Store>,
    Path(metric): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    stats_filtered(store, "_all".into(), Some(metric), p)
}

pub async fn stats_index_metric(
    State(store): State<Store>,
    Path((index, metric)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    stats_filtered(store, index, Some(metric), p)
}

/// `_stats/{metric}` narrows the report to the sections asked for.
pub(crate) fn stats_filtered(
    store: Store,
    expr: String,
    metric: Option<String>,
    p: Params,
) -> Response {
    let Some(metric) = metric else { return stats_impl(store, expr, p) };
    let wanted: Vec<String> = metric
        .split(',')
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
        // the section is called `merges`, and the metric may be asked for
        // in the singular
        .map(|m| if m == "merge" { "merges".to_string() } else { m.to_string() })
        .collect();
    for w in &wanted {
        if !STATS_METRICS.contains(&w.as_str()) {
            return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", {
                // a near miss is usually a typo, so the closest known
                // metric is offered rather than only the complaint
                let close = STATS_METRICS.iter().find(|m| one_edit_apart(m, w));
                match close {
                    Some(m) => format!(
                        "request [/_stats/{metric}] contains unrecognized metric: \
                             [{w}] -> did you mean [{m}]?"
                    ),
                    None => {
                        format!("request [/_stats/{metric}] contains unrecognized metric: [{w}]")
                    }
                }
            });
        }
    }
    if wanted.iter().any(|w| w == "_all") {
        return stats_impl(store, expr, p);
    }
    let body = match stats_value(&store, &expr, &p) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let keep = |section: &Value| -> Value {
        let mut out = serde_json::Map::new();
        if let Some(o) = section.as_object() {
            for w in &wanted {
                if let Some(v) = o.get(w) {
                    out.insert(w.clone(), v.clone());
                }
            }
        }
        Value::Object(out)
    };
    let mut filtered = body.clone();
    for scope in ["_all"] {
        for kind in ["primaries", "total"] {
            if let Some(v) = body.pointer(&format!("/{scope}/{kind}")) {
                filtered[scope][kind] = keep(v);
            }
        }
    }
    if let Some(indices) = body.get("indices").and_then(|v| v.as_object()) {
        for (name, entry) in indices {
            for kind in ["primaries", "total"] {
                if let Some(v) = entry.get(kind) {
                    filtered["indices"][name][kind] = keep(v);
                }
            }
        }
    }
    respond(&p, filtered)
}

pub async fn stats(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    stats_impl(store, index.map(|Path(i)| i).unwrap_or_else(|| "_all".into()), p)
}

pub(crate) fn stats_impl(store: Store, expr: String, p: Params) -> Response {
    match stats_value(&store, &expr, &p) {
        Ok(v) => respond(&p, v),
        Err(r) => r,
    }
}

pub(crate) fn stats_value(
    store: &Store,
    expr: &str,
    p: &Params,
) -> std::result::Result<Value, Response> {
    let targets = store.resolve(expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !ignore_unavailable(p) {
        return Err(no_such_index(expr));
    }
    let level = p.get("level").map(|s| s.as_str()).unwrap_or("indices");
    let want_groups: Option<Vec<String>> = p
        .get("groups")
        .map(|g| g.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect());
    let mut indices = serde_json::Map::new();
    let mut all = json!({});
    for n in &targets {
        let Some(st) = store.get(n) else { continue };
        let s = index_stats(
            &st.read(),
            store.index_size(n),
            want_groups.as_deref(),
            p,
            Some(&store.request_cache),
        );
        all = sum_stats(&all, &s);
        let mut entry = json!({
            "uuid": "_na_",
            "primaries": s.clone(),
            "total": s,
        });
        if level == "shards" {
            // each copy as the manager placed it, with the checkpoints the
            // primary tracks for it
            let live = crate::cluster::current_state();
            let me = crate::cluster::identity().id.clone();
            let seq_no = st.read().seq_no;
            let max_seq = seq_no as i64 - 1;
            let (local, global) = if seq_no == 0 {
                (-1i64, -1i64)
            } else {
                let (l, g) = crate::cluster::replication::checkpoints(n, seq_no - 1);
                (l as i64, g as i64)
            };
            let commit = json!({"id": st.read().commit_id(), "generation": 1, "user_data": {},
                "num_docs": s.pointer("/docs/count").cloned().unwrap_or(json!(0))});
            let seq = json!({"max_seq_no": max_seq, "local_checkpoint": local, "global_checkpoint": global});
            let mut shards = serde_json::Map::new();
            let mut copies: Vec<_> = live.routing.shards_of(n).collect();
            if copies.is_empty() {
                shards.insert("0".into(), json!([{
                    "routing": {"state": "STARTED", "primary": true, "node": me.as_str(), "relocating_node": null},
                    "docs": s.get("docs").cloned().unwrap_or(json!({})),
                    "commit": commit, "seq_no": seq,
                }]));
            } else {
                copies.sort_by_key(|c| (c.shard, !c.primary));
                for c in copies {
                    let list = shards.entry(c.shard.to_string()).or_insert_with(|| json!([]));
                    if let Some(a) = list.as_array_mut() {
                        a.push(json!({
                            "routing": {"state": c.state.as_str(), "primary": c.primary,
                                "node": c.node.as_ref().map(|x| x.as_str().to_string()),
                                "relocating_node": c.relocating_node.as_ref().map(|x| x.as_str().to_string())},
                            "docs": s.get("docs").cloned().unwrap_or(json!({})),
                            "commit": commit.clone(), "seq_no": seq.clone(),
                        }));
                    }
                }
            }
            entry["shards"] = Value::Object(shards);
        }
        indices.insert(n.clone(), entry);
    }
    let total_shards = shard_total(store, &targets);
    let mut body = json!({
        "_shards": {"total": total_shards, "successful": total_shards, "failed": 0},
        "_all": {"primaries": all.clone(), "total": all},
    });
    if level != "cluster" {
        body["indices"] = Value::Object(indices);
    }
    Ok(body)
}
