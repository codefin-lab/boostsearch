//! What this node is and what it is doing.

use super::*;

/// `_script_context` -- the places a script may run and what each hands it.
pub async fn script_contexts(Query(p): Query<Params>) -> Response {
    let ctx = |name: &str, ret: &str| {
        json!({
            "name": name,
            "methods": [{"name": "execute", "return_type": ret, "params": []}],
        })
    };
    respond(
        &p,
        json!({"contexts": [
            ctx("aggs", "double"),
            ctx("filter", "boolean"),
            ctx("score", "double"),
            ctx("update", "void"),
        ]}),
    )
}

/// `_script_language` -- which languages scripts may be written in, and how
/// they may be supplied.
pub async fn script_languages(Query(p): Query<Params>) -> Response {
    respond(
        &p,
        json!({
            "types_allowed": ["inline", "stored"],
            "language_contexts": [{
                "language": "painless",
                "contexts": ["aggs", "filter", "score", "update"],
            }],
        }),
    )
}

/// `_nodes/stats` -- what the one node has been doing.
pub async fn nodes_stats(
    State(store): State<Store>,
    rest: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    // the path may name which metrics are wanted, and a name that is not one
    // of them is a mistake rather than something to pass over
    const METRICS: &[&str] = &[
        "_all",
        "indices",
        "os",
        "process",
        "jvm",
        "thread_pool",
        "fs",
        "transport",
        "http",
        "breaker",
        "script",
        "discovery",
        "ingest",
        "adaptive_selection",
        "script_cache",
        "indexing_pressure",
        "shard_indexing_pressure",
        "search_backpressure",
        "cluster_manager_throttling",
        "weighted_routing",
        "resource_usage_stats",
        "segment_replication_backpressure",
        "repositories",
        "admission_control",
        "caches",
        "remote_store",
    ];
    // only the first path part names the metrics; anything after it narrows
    // within one, and is checked by whatever owns that metric
    let rest_parts: Vec<String> =
        rest.map(|Path(r)| r.split('/').map(|s| s.to_string()).collect()).unwrap_or_default();
    let asked: Vec<String> = rest_parts
        .first()
        .map(|r| r.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    for m in asked.iter().filter(|m| !m.is_empty() && *m != "stats") {
        // a node id is also allowed in this position, and ours is known
        if METRICS.contains(&m.as_str()) || matches!(m.as_str(), "node-0" | "_local" | "_all") {
            continue;
        }
        // a near miss is a typo, and naming the metric meant saves a reading
        // of the whole list
        let near = METRICS.iter().find(|k| {
            k.len().abs_diff(m.len()) <= 1
                && k.chars().filter(|c| m.contains(*c)).count() + 1 >= k.len()
        });
        let hint = match near {
            Some(k) => format!(" -> did you mean [{k}]?"),
            None => String::new(),
        };
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("request [/_nodes/stats/{m}] contains unrecognized metric: [{m}]{hint}"),
        );
    }
    let mut docs = 0u64;
    for n in store.names() {
        if let Some(st) = store.get(&n) {
            docs += st.read().reader.searcher().num_docs();
        }
    }
    // a second path part narrows within `indices` to the metrics it names
    let index_metrics: Vec<String> = rest_parts
        .get(1)
        .map(|r| r.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    let zero_time = json!({"total": 0, "time_in_millis": 0, "current": 0});
    let mut out = json!({
        "_nodes": {"total": 1, "successful": 1, "failed": 0},
        "cluster_name": "boostsearch",
        "nodes": {"node-0": {
            "timestamp": 0, "name": "boostsearch",
            "transport_address": "127.0.0.1:9300", "host": "127.0.0.1", "ip": "127.0.0.1",
            "roles": ["cluster_manager", "data", "ingest"], "attributes": {},
            "indices": {
                "docs": {"count": docs, "deleted": 0},
                "store": {"size_in_bytes": 0, "reserved_in_bytes": 0},
                "indexing": {
                    "index_total": docs, "index_time_in_millis": 0, "index_current": 0,
                    "index_failed": 0, "delete_total": 0, "delete_time_in_millis": 0,
                    "delete_current": 0, "noop_update_total": 0, "is_throttled": false,
                    "throttle_time_in_millis": 0,
                    "doc_status": {},
                },
                // how many writes ended in each status class, which is what a
                // caller watching for rejections reads
                "status_counter": {
                    "doc_status": {"1xx": 0, "2xx": 0, "3xx": 0, "4xx": 0, "5xx": 0},
                    "search_response_status": {
                        "1xx": 0, "2xx": 0, "3xx": 0, "4xx": 0, "5xx": 0
                    },
                },
                "get": {"total": 0, "time_in_millis": 0, "exists_total": 0,
                        "exists_time_in_millis": 0, "missing_total": 0,
                        "missing_time_in_millis": 0, "current": 0},
                "search": {"open_contexts": 0, "query_total": 0, "query_time_in_millis": 0,
                           "query_current": 0, "fetch_total": 0, "fetch_time_in_millis": 0,
                           "fetch_current": 0, "scroll_total": 0, "scroll_time_in_millis": 0,
                           "scroll_current": 0, "suggest_total": 0,
                           "suggest_time_in_millis": 0, "suggest_current": 0},
                "merges": {"current": 0, "current_docs": 0, "current_size_in_bytes": 0,
                           "total": 0, "total_time_in_millis": 0, "total_docs": 0,
                           "total_size_in_bytes": 0},
                "refresh": {"total": 0, "total_time_in_millis": 0, "external_total": 0,
                            "external_total_time_in_millis": 0, "listeners": 0},
                "flush": {"total": 0, "periodic": 0, "total_time_in_millis": 0},
                "warmer": zero_time.clone(),
                "query_cache": {"memory_size_in_bytes": 0, "total_count": 0,
                                "hit_count": 0, "miss_count": 0, "cache_size": 0,
                                "cache_count": 0, "evictions": 0},
                "fielddata": {"memory_size_in_bytes": 0, "evictions": 0},
                "completion": {"size_in_bytes": 0},
                "segments": {"count": 0, "memory_in_bytes": 0},
                "translog": {"operations": 0, "size_in_bytes": 0,
                             "uncommitted_operations": 0, "uncommitted_size_in_bytes": 0,
                             "earliest_last_modified_age": 0},
                "request_cache": {"memory_size_in_bytes": 0, "evictions": 0,
                                  "hit_count": 0, "miss_count": 0},
                "recovery": {"current_as_source": 0, "current_as_target": 0,
                             "throttle_time_in_millis": 0},
            },
            "os": {"timestamp": 0, "cpu": {"percent": 0}, "mem": {
                "total_in_bytes": 1_073_741_824u64, "free_in_bytes": 536_870_912u64,
                "used_in_bytes": 536_870_912u64, "free_percent": 50, "used_percent": 50}},
            "process": {"timestamp": 0, "open_file_descriptors": 0,
                        "max_file_descriptors": 0,
                        "cpu": {"percent": 0, "total_in_millis": 0},
                        "mem": {"total_virtual_in_bytes": 0}},
            "jvm": {"timestamp": 0, "uptime_in_millis": 0,
                    "mem": {"heap_used_in_bytes": 0, "heap_max_in_bytes": 0},
                    "threads": {"count": 1, "peak_count": 1}},
            "thread_pool": {}, "fs": {"total": {
                "total_in_bytes": 2_147_483_648u64, "free_in_bytes": 1_073_741_824u64,
                "available_in_bytes": 1_073_741_824u64}},
            "transport": {"server_open": 0, "rx_count": 0, "rx_size_in_bytes": 0,
                          "tx_count": 0, "tx_size_in_bytes": 0},
            "http": {"current_open": 0, "total_opened": 0},
            "breakers": {}, "script": {"compilations": 0, "cache_evictions": 0},
            "discovery": {}, "ingest": crate::api::ingest_stats_json(&store),
            "adaptive_selection": {}, "script_cache": {"sum": {}},
            "indexing_pressure": {"memory": {}},
        }},
    });
    if !index_metrics.is_empty()
        && !index_metrics.iter().any(|m| m == "_all")
        && let Some(idx) = out.pointer_mut("/nodes/node-0/indices").and_then(|v| v.as_object_mut())
    {
        // the status counter belongs to indexing, and travels with it
        idx.retain(|k, _| {
            index_metrics.iter().any(|m| m == k)
                || (k == "status_counter" && index_metrics.iter().any(|m| m == "indexing"))
        });
    }
    respond(&p, out)
}

/// What the process is actually holding, and where.
///
/// `?collect=true` first asks the allocator to hand back everything it can, so
/// the difference between the two answers separates "retained by the allocator"
/// from "still referenced by us".
pub async fn memory_report(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    if flag(&p, "collect") {
        unsafe { libmimalloc_sys::mi_collect(true) };
    }
    let (
        mut elapsed,
        mut user,
        mut sys,
        mut rss,
        mut peak_rss,
        mut commit,
        mut peak_commit,
        mut faults,
    ) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    unsafe {
        libmimalloc_sys::mi_process_info(
            &mut elapsed,
            &mut user,
            &mut sys,
            &mut rss,
            &mut peak_rss,
            &mut commit,
            &mut peak_commit,
            &mut faults,
        );
    }
    let mb = |v: usize| (v as f64 / 1_048_576.0 * 10.0).round() / 10.0;

    let mut per_index = Vec::new();
    let (mut live_ids, mut versions, mut pending, mut shapes, mut kinds, mut segments, mut writers) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    for name in store.names() {
        let Some(st) = store.get(&name) else { continue };
        let g = st.read();
        let segs = g.reader.searcher().segment_readers().len();
        live_ids += g.live_ids.len();
        versions += g.versions.len();
        pending += g.pending.len();
        shapes += g.seen_shapes.len();
        kinds += g.observed_kinds.len();
        segments += segs;
        if g.has_writer() {
            writers += 1;
        }
        if per_index.len() < 3 {
            per_index.push(json!({
                "index": name, "segments": segs, "live_ids": g.live_ids.len(),
                "versions": g.versions.len(), "pending": g.pending.len(),
                "pending_bytes": g.pending_bytes, "has_writer": g.has_writer(),
            }));
        }
    }
    respond(
        &p,
        json!({
            "allocator": {
                "rss_mb": mb(rss), "peak_rss_mb": mb(peak_rss),
                "committed_mb": mb(commit), "peak_committed_mb": mb(peak_commit),
                "page_faults": faults,
            },
            "indices": {
                "count": store.names().len(), "live_writers": writers,
                "total_segments": segments, "total_live_ids": live_ids,
                "total_versions": versions, "total_pending": pending,
                "total_shapes": shapes, "total_kind_paths": kinds,
            },
            "sample": per_index,
        }),
    )
}

/// `_list/wlm_stats` -- workload group statistics as a table.
///
/// This engine runs one node and does not divide work between workload
/// groups, so there is one group with nothing rejected. The parameters are
/// still checked, since a caller paging through the list needs to be told
/// when it has asked for something the list cannot give.
pub async fn wlm_stats_list(Query(p): Query<Params>) -> Response {
    let bad = |reason: String| {
        (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "error": {"type": "illegal_argument_exception", "reason": reason},
                "status": 400,
            })),
        )
            .into_response()
    };
    if let Some(sort) = p.get("sort")
        && !matches!(sort.as_str(), "node_id" | "workload_group")
    {
        return bad("Invalid value for 'sort'. Allowed: 'node_id', 'workload_group'".into());
    }
    if let Some(order) = p.get("order")
        && !matches!(order.as_str(), "asc" | "desc")
    {
        return bad("Invalid value for 'order'. Allowed: 'asc', 'desc'".into());
    }
    if let Some(size) = p.get("size") {
        let n = size.parse::<i64>().unwrap_or(-1);
        if !(1..=100).contains(&n) {
            return bad("Invalid value for 'size'. Allowed range: 1 to 100".into());
        }
    }
    if p.contains_key("next_token") {
        // there is one page and it never moves, so any token names a state
        // that this list cannot have been in
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "error": "Pagination state has changed (e.g., new workload groups added or \
                          removed). Please restart pagination from the beginning by omitting \
                          the 'next_token' parameter.",
                "status": 400,
            })),
        )
            .into_response();
    }
    let cols = [
        "NODE_ID",
        "WORKLOAD_GROUP_ID",
        "TOTAL_COMPLETIONS",
        "TOTAL_REJECTIONS",
        "TOTAL_CANCELLATIONS",
        "CPU_USAGE",
        "MEMORY_USAGE",
    ];
    let row = ["node-0", "DEFAULT_WORKLOAD_GROUP", "0", "0", "0", "0", "0"];
    let mut out = String::new();
    if p.get("v").map(|v| v != "false").unwrap_or(false) {
        out.push_str(&cols.join(" "));
        out.push('\n');
    }
    out.push_str(&row.join(" "));
    out.push('\n');
    ([("content-type", "text/plain; charset=UTF-8")], out).into_response()
}

/// The attributes this node was started with: the built-in one, and whatever
/// `BOOSTSEARCH_NODE_ATTRS` named, as `name=value` pairs separated by commas.
/// The OpenSearch modules this server answers for.
///
/// A module is not a thing loaded here -- everything is built in -- but the
/// suites ask which of them a node carries before they test what one does, so
/// the list names the ones whose behaviour is answered rather than the ones
/// whose code is present.
fn modules() -> Value {
    const NAMED: &[&str] = &[
        "aggs-matrix-stats",
        "analysis-common",
        "geo",
        "ingest-common",
        "ingest-geoip",
        "ingest-user-agent",
        "lang-mustache",
        "lang-painless",
        "mapper-extras",
        "opensearch-dashboards",
        "parent-join",
        "percolator",
        "rank-eval",
        "reindex",
        "search-pipeline-common",
        "transport-netty4",
    ];
    Value::Array(
        NAMED
            .iter()
            .map(|name| {
                json!({
                    "name": name,
                    "version": "3.9.0",
                    "opensearch_version": "3.9.0",
                    "java_version": "11",
                    "description": format!("the {name} module"),
                    "classname": "",
                    "custom_foldername": "",
                    "extended_plugins": [],
                    "has_native_controller": false,
                })
            })
            .collect(),
    )
}

pub fn node_attrs() -> Vec<(String, String)> {
    let mut out = vec![("shard_indexing_pressure_enabled".to_string(), "true".to_string())];
    if let Ok(spec) = std::env::var("BOOSTSEARCH_NODE_ATTRS") {
        for pair in spec.split(',') {
            if let Some((k, v)) = pair.split_once('=') {
                let (k, v) = (k.trim(), v.trim());
                if !k.is_empty() {
                    out.push((k.to_string(), v.to_string()));
                }
            }
        }
    }
    out
}

pub async fn nodes_info(Query(p): Query<Params>) -> Response {
    respond(
        &p,
        json!({
            "_nodes": {"total": 1, "successful": 1, "failed": 0},
            "cluster_name": "boostsearch",
            "nodes": {"node-0": {
                "name": "boostsearch", "transport_address": "127.0.0.1:9300",
                "host": "127.0.0.1", "ip": "127.0.0.1", "version": "3.9.0",
                "build_type": "tar", "build_hash": "boostsearch", "roles": ["data", "ingest"],
                "attributes": {},
                "os": {"refresh_interval_in_millis": 1000,
                       "available_processors": num_cpus(),
                       "allocated_processors": num_cpus()},
                "process": {"refresh_interval_in_millis": 1000, "id": std::process::id(),
                            "mlockall": false},
                "plugins": [], "modules": modules(), "ingest": {"processors": crate::ingest::PROCESSOR_TYPES.iter().map(|t| json!({"type": t})).collect::<Vec<_>>()},
                "search_pipelines": {
                    "request_processors": crate::search::pipeline::REQUEST_PROCESSORS.iter().map(|t| json!({"type": t})).collect::<Vec<_>>(),
                    "response_processors": crate::search::pipeline::RESPONSE_PROCESSORS.iter().map(|t| json!({"type": t})).collect::<Vec<_>>(),
                },
                "thread_pool": {}, "transport": {},
                // where a client -- or another cluster reindexing from this
                // one -- reaches this node
                "http": {
                    "bound_address": [crate::api::bound_address()],
                    "publish_address": crate::api::bound_address(),
                    "max_content_length_in_bytes": crate::api::max_content_bytes(),
                },
            }},
        }),
    )
}
