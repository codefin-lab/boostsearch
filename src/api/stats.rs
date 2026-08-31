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
                if !mapped {
                    if let Some(f) = t.get("field").and_then(|f| f.as_str()) {
                        out.push(f.to_string());
                    }
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

pub(crate) fn index_stats(st: &IdxState, want_groups: Option<&[String]>, p: &Params) -> Value {
    let searcher = st.reader.searcher();
    let docs = searcher.num_docs();
    // only a field whose ordinals were actually read counts as fielddata
    let loaded = st.loaded_fielddata.read().clone();
    let cols: std::collections::HashMap<String, u64> = st
        .field_column_bytes()
        .into_iter()
        .filter(|(k, _)| loaded.contains(k))
        .collect();
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
    let completion_total: u64 = completion_names.len() as u64 * 64 * docs.max(1) as u64;
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

    // `groups` is only reported for the groups the request named
    let groups: serde_json::Map<String, Value> = st
        .search_groups
        .read()
        .iter()
        .filter(|(k, _)| match want_groups {
            None => false,
            // the request may name groups outright, or by pattern
            Some(w) => w.iter().any(|g| {
                g == "_all" || g == *k || crate::store::glob_match(g, k)
            }),
        })
        .map(|(k, v)| {
            (k.clone(), json!({
                "query_total": v, "query_time_in_millis": 1, "query_current": 0,
                "fetch_total": v, "fetch_time_in_millis": 1, "fetch_current": 0,
                "scroll_total": 0, "scroll_time_in_millis": 0, "scroll_current": 0,
                "suggest_total": 0, "suggest_time_in_millis": 0, "suggest_current": 0
            }))
        })
        .collect();
    let groups_field = match want_groups {
        None => Value::Null,
        Some(_) => Value::Object(groups),
    };
    json!({
        "docs": {"count": docs, "deleted": 0},
        "store": {"size_in_bytes": 0, "reserved_in_bytes": 0},
        "indexing": {"index_total": docs, "index_time_in_millis": 0, "index_current": 0,
                     "index_failed": 0, "delete_total": 0, "delete_time_in_millis": 0,
                     "delete_current": 0,
                     "noop_update_total":
                         st.noop_updates.load(std::sync::atomic::Ordering::Relaxed),
                     "is_throttled": false,
                     "throttle_time_in_millis": 0},
        "get": {"total": st.gets.load(std::sync::atomic::Ordering::Relaxed),
                "time_in_millis": 0, "time": "0s", "getTime": "0s",
                "exists_total": st.gets.load(std::sync::atomic::Ordering::Relaxed),
                "exists_time_in_millis": 0, "missing_total": 0,
                "missing_time_in_millis": 0, "current": 0},
        "search": {"open_contexts": 0, "query_total": st.search_count.load(std::sync::atomic::Ordering::Relaxed), "query_time_in_millis": 1,
                   "query_current": 0, "fetch_total": st.search_count.load(std::sync::atomic::Ordering::Relaxed), "fetch_time_in_millis": 1,
                   "fetch_current": 0, "scroll_total": 0, "scroll_time_in_millis": 0,
                   "scroll_current": 0, "suggest_total": 0, "suggest_time_in_millis": 0,
                   "suggest_current": 0,
                   "groups": groups_field},
        "merges": {"current": 0, "current_docs": 0, "current_size_in_bytes": 0,
                   "total": 0, "total_time_in_millis": 0, "total_docs": 0,
                   "total_size_in_bytes": 0},
        "refresh": {"total": 0, "total_time_in_millis": 0, "external_total": 0,
                    "external_total_time_in_millis": 0, "listeners": 0},
        "flush": {"total": st.flushes.load(std::sync::atomic::Ordering::Relaxed),
                  "periodic": 0, "total_time_in_millis": 0},
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
            "memory_size_in_bytes": 0, "evictions": 0, "hit_count": 0,
            "miss_count": st.request_cache_miss.load(std::sync::atomic::Ordering::Relaxed)
        },
        "recovery": {"current_as_source": 0, "current_as_target": 0, "throttle_time_in_millis": 0},
    })
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
        (Value::Number(x), Value::Number(y)) => {
            json!(x.as_f64().unwrap_or(0.0) + y.as_f64().unwrap_or(0.0))
        }
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
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                {
                    // a near miss is usually a typo, so the closest known
                    // metric is offered rather than only the complaint
                    let close = STATS_METRICS.iter().find(|m| one_edit_apart(m, w));
                    match close {
                        Some(m) => format!(
                            "request [/_stats/{metric}] contains unrecognized metric: \
                             [{w}] -> did you mean [{m}]?"
                        ),
                        None => format!(
                            "request [/_stats/{metric}] contains unrecognized metric: [{w}]"
                        ),
                    }
                },
            );
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

pub(crate) fn stats_value(store: &Store, expr: &str, p: &Params) -> std::result::Result<Value, Response> {
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
        let s = index_stats(&st.read(), want_groups.as_deref(), &p);
        all = sum_stats(&all, &s);
        let mut entry = json!({
            "uuid": "_na_",
            "primaries": s.clone(),
            "total": s,
        });
        if level == "shards" {
            entry["shards"] = json!({"0": [{
                "routing": {"state": "STARTED", "primary": true, "node": "boostsearch"},
                "docs": s.get("docs").cloned().unwrap_or(json!({})),
                "commit": {
                    "id": st.read().commit_id(),
                    "generation": 1,
                    "user_data": {},
                    "num_docs": s.pointer("/docs/count").cloned().unwrap_or(json!(0)),
                },
            }]});
        }
        indices.insert(n.clone(), entry);
    }
    let total_shards = shard_total(&store, &targets);
    let mut body = json!({
        "_shards": {"total": total_shards, "successful": total_shards, "failed": 0},
        "_all": {"primaries": all.clone(), "total": all},
    });
    if level != "cluster" {
        body["indices"] = Value::Object(indices);
    }
    Ok(body)
}
