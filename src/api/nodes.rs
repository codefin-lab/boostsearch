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
        if METRICS.contains(&m.as_str())
            || matches!(m.as_str(), "_local" | "_all")
            || m == crate::cluster::identity().id.as_str()
            || *m == crate::cluster::identity().name
        {
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
    // a second path part narrows within `indices` to the metrics it names
    let index_metrics: Vec<String> = rest_parts
        .get(1)
        .map(|r| r.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    let me = crate::cluster::identity();
    let level = p.get("level").map(|s| s.as_str()).unwrap_or("node");
    if !matches!(level, "node" | "indices" | "shards") {
        // the reference fails the node rather than the request
        return respond(
            &p,
            json!({
                "_nodes": {"total": 1, "successful": 0, "failed": 1, "failures": [{
                    "type": "failed_node_exception",
                    "reason": format!("Failed node [{}]", me.id.as_str()),
                    "node_id": me.id.as_str(),
                    "caused_by": {"type": "illegal_argument_exception",
                                  "reason": "Level provided is not supported by NodeIndicesStats"},
                }]},
                "cluster_name": me.cluster_name,
                "nodes": {},
            }),
        );
    }
    let narrow = |mut v: Value| -> Value {
        if index_metrics.is_empty() || index_metrics.iter().any(|m| m == "_all") {
            return v;
        }
        if let Some(o) = v.as_object_mut() {
            o.retain(|k, _| index_metrics.iter().any(|m| m == k));
        }
        v
    };
    let indices = node_indices_stats(&store, &p, level, &narrow, &index_metrics);
    let mut local = json!({
        "timestamp": crate::store::now_millis(), "name": me.name,
        "transport_address": me.transport_address,
        "host": me.host, "ip": me.host,
        "roles": me.roles, "attributes": me.attributes,
        "indices": indices,
    });
    if let (Some(o), Value::Object(machine)) = (local.as_object_mut(), machine_stats(&store)) {
        o.extend(machine);
        o.insert("ingest".into(), crate::api::ingest_stats_json(&store));
    }
    // the metrics the path named, and what says which node this is
    let wanted: Vec<String> = asked
        .iter()
        .filter(|m| METRICS.contains(&String::as_str(m)))
        .map(|m| if m == "breaker" { "breakers".to_string() } else { m.clone() })
        .collect();
    if !wanted.is_empty()
        && !wanted.iter().any(|m| m == "_all")
        && let Some(o) = local.as_object_mut()
    {
        const IDENTITY: &[&str] =
            &["timestamp", "name", "transport_address", "host", "ip", "roles", "attributes"];
        o.retain(|k, _| IDENTITY.contains(&k.as_str()) || wanted.iter().any(|m| m == k));
    }
    let mut out = json!({
        "_nodes": {"total": 1, "successful": 1, "failed": 0},
        "cluster_name": me.cluster_name,
        "nodes": {me.id.as_str(): local},
    });
    // every other node the cluster holds, with what its identity says
    {
        let live = crate::cluster::current_state();
        let me = crate::cluster::identity();
        if let Some(nodes) = out.get_mut("nodes").and_then(|n| n.as_object_mut()) {
            for (id, n) in &live.nodes {
                if *id == me.id {
                    continue;
                }
                let ip = n
                    .transport_address
                    .rsplit_once(':')
                    .map(|(h, _)| h.to_string())
                    .unwrap_or_default();
                nodes.insert(
                    id.as_str().to_string(),
                    json!({
                        "timestamp": 0, "name": n.name, "transport_address": n.transport_address,
                        "host": ip, "ip": ip, "roles": n.roles, "attributes": n.attributes,
                    }),
                );
            }
        }
        out["_nodes"] = json!({"total": live.nodes.len().max(1), "successful": live.nodes.len().max(1), "failed": 0});
    }
    respond(&p, out)
}

/// The `indices` section of a node's statistics: every index it holds summed,
/// and at `level=indices` or `level=shards` each of them on its own.
///
/// It reported the live documents and their size, and zero for everything
/// else -- `search.query_total` and `segments.count` read 0 at node level
/// while the same indices counted them in `_stats`.
fn node_indices_stats(
    store: &Store,
    p: &Params,
    level: &str,
    narrow: &dyn Fn(Value) -> Value,
    index_metrics: &[String],
) -> Value {
    let mut total = json!({});
    let mut each = serde_json::Map::new();
    let me = crate::cluster::identity();
    for n in store.names() {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        // the request cache is one for the whole node, so it is added once
        // below rather than once for every index
        let s = crate::api::index_stats(&g, store.index_size(&n), None, p, None);
        total = crate::api::sum_stats(&total, &s);
        match level {
            "indices" => {
                each.insert(n.clone(), narrow(s));
            }
            "shards" => {
                let mut shard = narrow(s);
                shard["routing"] = json!({"state": "STARTED", "primary": true,
                    "node": me.id.as_str(), "relocating_node": null});
                each.insert(n.clone(), json!([{"0": shard}]));
            }
            _ => {}
        }
    }
    if total.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        // a node holding no index still reports every section, at zero
        let empty = crate::store::Store::scratch();
        if let Ok(st) = empty.ensure("_empty") {
            total = crate::api::index_stats(&st.read(), 0, None, p, None);
            total["translog"]["size_in_bytes"] = json!(0);
            total["translog"]["uncommitted_size_in_bytes"] = json!(0);
        }
    }
    total["request_cache"]["memory_size_in_bytes"] = json!(store.request_cache.bytes());
    total["request_cache"]["evictions"] =
        json!(store.request_cache.evictions.load(std::sync::atomic::Ordering::Relaxed));
    let num = |v: &Value, ptr: &str| v.pointer(ptr).and_then(|x| x.as_u64()).unwrap_or(0);
    // how many writes and searches ended well and how many did not, which is
    // what a caller watching for rejections reads
    let status_counter = json!({
        "doc_status": {
            "success": num(&total, "/indexing/index_total") + num(&total, "/indexing/delete_total"),
            "user_error": num(&total, "/indexing/index_failed"),
            "system_failure": 0,
        },
        "search_response_status": {
            "success": num(&total, "/search/query_total"),
            "user_error": num(&total, "/search/query_failed"),
            "system_failure": 0,
        },
    });
    // the phases of the search requests this node coordinated, which it
    // reports beside the shard-level counts
    if total.get("search").is_some() {
        let phase = |t: &str, ms: &str| json!({"time_in_millis": num(&total, ms), "current": 0, "total": num(&total, t)});
        let none = json!({"time_in_millis": 0, "current": 0, "total": 0});
        total["search"]["request"] = json!({
            "took": {
                "time_in_millis": num(&total, "/search/query_time_in_millis")
                    + num(&total, "/search/fetch_time_in_millis"),
                "current": 0,
                "total": num(&total, "/search/query_total"),
            },
            "dfs_pre_query": none, "dfs_query": none, "can_match": none,
            "query": phase("/search/query_total", "/search/query_time_in_millis"),
            "fetch": phase("/search/fetch_total", "/search/fetch_time_in_millis"),
            "expand": phase("/search/fetch_total", "/search/nothing"),
        });
    }
    let mut out = narrow(total);
    // the status counter belongs to indexing, and travels with it
    if index_metrics.is_empty() || index_metrics.iter().any(|m| m == "indexing" || m == "_all") {
        out["status_counter"] = status_counter;
    }
    match level {
        "indices" => out["indices"] = Value::Object(each),
        "shards" => out["shards"] = Value::Object(each),
        _ => {}
    }
    out
}

/// What the machine and this process are using: the `os`, `process`, `jvm`
/// and `fs` sections and the rest of a node's statistics, read from the
/// operating system each time they are asked for.
fn machine_stats(store: &Store) -> Value {
    use crate::api::sysinfo;
    let now = crate::store::now_millis();
    let pct = |part: u64, whole: u64| -> u64 {
        if whole == 0 { 0 } else { ((part as f64 / whole as f64) * 100.0).round() as u64 }
    };
    let mem = sysinfo::memory();
    let used = mem.total.saturating_sub(mem.free);
    let swap = sysinfo::swap();
    let load = sysinfo::load_average().unwrap_or([0.0; 3]);
    let round2 = |v: f64| (v * 100.0).round() / 100.0;
    let proc_ = sysinfo::process();
    let (alloc_resident, _peak) = sysinfo::allocator();
    let data = store
        .data_dir()
        .map(|d| d.to_path_buf())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    let disk = sysinfo::disk(&data);
    let (disk_total, disk_free, disk_avail) =
        disk.as_ref().map(|d| (d.total, d.free, d.available)).unwrap_or((0, 0, 0));
    let mut pools = serde_json::Map::new();
    for pool in crate::api::pools::POOLS.iter() {
        let mut entry = json!({
            "threads": pool.threads(), "queue": pool.queue(), "active": pool.active(),
            "rejected": pool.rejected(), "largest": pool.largest(),
            "completed": pool.completed(),
        });
        if pool.name == "search" || pool.name == "index_searcher" {
            entry["total_wait_time_in_nanos"] = json!(0);
        }
        pools.insert(pool.name.to_string(), entry);
    }
    let breaker = |limit: u64, overhead: f64| {
        json!({"limit_size_in_bytes": limit,
               "limit_size": crate::api::shared::sized(None, limit),
               "estimated_size_in_bytes": 0, "estimated_size": "0b",
               "overhead": overhead, "tripped": 0})
    };
    let zero_pool = json!({"used_in_bytes": 0, "max_in_bytes": 0, "peak_used_in_bytes": 0,
        "peak_max_in_bytes": 0,
        "last_gc_stats": {"used_in_bytes": 0, "max_in_bytes": 0, "usage_percent": -1}});
    let tracker =
        json!({"cancellation_count": 0, "current_max_millis": 0, "current_avg_millis": 0});
    let task_backpressure = json!({
        "resource_tracker_stats": {
            "heap_usage_tracker": {"cancellation_count": 0, "current_max_bytes": 0,
                                   "current_avg_bytes": 0, "rolling_avg_bytes": 0},
            "elapsed_time_tracker": tracker, "cpu_usage_tracker": tracker,
        },
        "completion_count": 0,
        "cancellation_stats": {"cancellation_count": 0, "cancellation_limit_reached_count": 0},
    });
    let me = crate::cluster::identity();
    let io_zero = json!({"transport": {"rejection_count": {}}});
    let pressure_zero = json!({"combined_coordinating_and_primary_in_bytes": 0,
        "coordinating_in_bytes": 0, "primary_in_bytes": 0, "replica_in_bytes": 0,
        "all_in_bytes": 0});
    let mut pressure_total = pressure_zero.clone();
    pressure_total["coordinating_rejections"] = json!(0);
    pressure_total["primary_rejections"] = json!(0);
    pressure_total["replica_rejections"] = json!(0);
    let timing_zero = json!({"count": 0, "time_in_millis": 0, "current": 0, "failed": 0});
    json!({
        "os": {
            "timestamp": now,
            "cpu": {"percent": sysinfo::os_cpu_percent(),
                    "load_average": {"1m": round2(load[0]), "5m": round2(load[1]),
                                     "15m": round2(load[2])}},
            "mem": {"total_in_bytes": mem.total, "free_in_bytes": mem.free,
                    "used_in_bytes": used, "free_percent": pct(mem.free, mem.total),
                    "used_percent": pct(used, mem.total)},
            "swap": {"total_in_bytes": swap.total, "free_in_bytes": swap.free,
                     "used_in_bytes": swap.total.saturating_sub(swap.free)},
        },
        "process": {
            "timestamp": now,
            "open_file_descriptors": proc_.open_fds, "max_file_descriptors": proc_.max_fds,
            "cpu": {"percent": sysinfo::process_cpu_percent(), "total_in_millis": proc_.cpu_millis},
            "mem": {"total_virtual_in_bytes": proc_.virtual_size},
        },
        // There is no JVM and no garbage-collected heap. What the allocator
        // holds is the nearest thing, and it is what these report:
        // `heap_used` and `heap_committed` are the bytes the allocator holds
        // resident for the node's own data, and `heap_max` the machine's
        // memory, which is as far as it can grow. What the process holds
        // besides -- mapped index files, thread stacks -- is `non_heap`. A
        // dashboard's heap gauge then shows the memory the node's own data
        // takes, which is what it is watched for.
        "jvm": {
            "timestamp": now, "uptime_in_millis": sysinfo::uptime_millis(),
            "mem": {
                "heap_used_in_bytes": alloc_resident,
                "heap_used_percent": pct(alloc_resident, mem.total),
                "heap_committed_in_bytes": alloc_resident,
                "heap_max_in_bytes": mem.total,
                "non_heap_used_in_bytes": proc_.resident.saturating_sub(alloc_resident),
                "non_heap_committed_in_bytes": proc_.resident.saturating_sub(alloc_resident),
                "pools": {"young": zero_pool, "old": zero_pool, "survivor": zero_pool},
            },
            "threads": {"count": proc_.threads, "peak_count": sysinfo::peak_threads(proc_.threads)},
            "gc": {"collectors": {
                "young": {"collection_count": 0, "collection_time_in_millis": 0},
                "old": {"collection_count": 0, "collection_time_in_millis": 0},
            }},
            "buffer_pools": {
                "mapped": {"count": 0, "used_in_bytes": 0, "total_capacity_in_bytes": 0},
                "direct": {"count": 0, "used_in_bytes": 0, "total_capacity_in_bytes": 0},
            },
            "classes": {"current_loaded_count": 0, "total_loaded_count": 0,
                        "total_unloaded_count": 0},
        },
        "thread_pool": Value::Object(pools),
        "fs": {
            "timestamp": now,
            "total": {"total_in_bytes": disk_total, "free_in_bytes": disk_free,
                      "available_in_bytes": disk_avail, "cache_reserved_in_bytes": 0},
            "data": disk.as_ref().map(|d| vec![json!({
                "path": d.path, "mount": d.mount, "type": d.kind,
                "total_in_bytes": d.total, "free_in_bytes": d.free,
                "available_in_bytes": d.available, "cache_reserved_in_bytes": 0,
            })]).unwrap_or_default(),
            "io_stats": {},
        },
        "transport": {"server_open": 0, "total_outbound_connections": 0, "rx_count": 0,
                      "rx_size_in_bytes": 0, "tx_count": 0, "tx_size_in_bytes": 0},
        "http": {"current_open": 0, "total_opened": 0},
        // the limits the reference derives from its heap, derived here from
        // what stands for it
        "breakers": {
            "request": breaker(mem.total * 60 / 100, 1.0),
            "fielddata": breaker(mem.total * 40 / 100, 1.03),
            "in_flight_requests": breaker(mem.total, 2.0),
            "parent": breaker(mem.total * 95 / 100, 1.0),
        },
        "script": {"compilations": 0, "cache_evictions": 0, "compilation_limit_triggered": 0},
        "discovery": {
            "cluster_state_queue": {"total": 0, "pending": 0, "committed": 0},
            "published_cluster_states": {"full_states": 0, "incompatible_diffs": 0,
                                         "compatible_diffs": 0},
            "cluster_state_stats": {"overall": {"update_count": 0, "total_time_in_millis": 0,
                                                "failed_count": 0}},
        },
        "adaptive_selection": {},
        "script_cache": {
            "sum": {"compilations": 0, "cache_evictions": 0, "compilation_limit_triggered": 0},
            "contexts": [],
        },
        "indexing_pressure": {"memory": {
            "current": pressure_zero, "total": pressure_total, "limit_in_bytes": mem.total / 10,
        }},
        "shard_indexing_pressure": {
            "stats": {},
            "total_rejections_breakup_shadow_mode": {"node_limits": 0,
                "no_successful_request_limits": 0, "throughput_degradation_limits": 0},
            "enabled": false, "enforced": false,
        },
        "search_backpressure": {
            "search_task": task_backpressure.clone(), "search_shard_task": task_backpressure,
            "mode": "monitor_only",
        },
        "cluster_manager_throttling": {"stats": {"total_throttled_tasks": 0,
                                                  "throttled_tasks_per_task_type": {}}},
        "weighted_routing": {"stats": {"fail_open_count": 0}},
        "task_cancellation": {
            "search_task": {"current_count_post_cancel": 0, "total_count_post_cancel": 0},
            "search_shard_task": {"current_count_post_cancel": 0, "total_count_post_cancel": 0},
        },
        "search_pipeline": {
            "total_request": timing_zero.clone(), "total_response": timing_zero,
            "pipelines": {},
        },
        "resource_usage_stats": {me.id.as_str(): {
            "timestamp": now,
            "cpu_utilization_percent": format!("{:.1}", sysinfo::process_cpu_percent() as f64),
            "memory_utilization_percent": format!("{:.1}", pct(alloc_resident, mem.total) as f64),
            "io_usage_stats": {"max_io_utilization_percent": "0.0"},
        }},
        "segment_replication_backpressure": {"total_rejected_requests": 0},
        "repositories": [],
        "admission_control": {"global_io_usage": io_zero.clone(), "global_cpu_usage": io_zero},
        "caches": {"request_cache": {
            "size_in_bytes": store.request_cache.bytes(),
            "evictions": store.request_cache.evictions.load(std::sync::atomic::Ordering::Relaxed),
            "hit_count": 0, "miss_count": 0, "item_count": 0, "store_name": "opensearch_onheap",
        }},
        "remote_store": {"last_successful_fetch_of_pinned_timestamps": -1},
        "native_memory": {"total_estimated_bytes": proc_.resident},
    })
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
    let me = crate::cluster::identity();
    let row = [me.id.as_str(), "DEFAULT_WORKLOAD_GROUP", "0", "0", "0", "0", "0"];
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
/// The plugins a distribution of OpenSearch would install, and this engine
/// answers for without one. Reported for the same reason `_cat/plugins`
/// reports them: a client asking whether it may use `attachment` should be
/// told the truth.
fn plugins() -> Value {
    const NAMED: &[&str] = &[
        "analysis-icu",
        // only where the dictionaries were built in; see `_cat/plugins`
        #[cfg(feature = "cjk")]
        "analysis-kuromoji",
        #[cfg(feature = "cjk")]
        "analysis-nori",
        "analysis-phonenumber",
        "analysis-phonetic",
        #[cfg(feature = "cjk")]
        "analysis-smartcn",
        "analysis-stempel",
        "analysis-ukrainian",
        "ingest-attachment",
        "opensearch-index-management",
        "opensearch-knn",
        "opensearch-security",
        "opensearch-sql",
        "repository-azure",
        "repository-gcs",
        "repository-s3",
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
                    "description": format!("the {name} plugin"),
                    "classname": "",
                    "custom_foldername": "",
                    "extended_plugins": [],
                    "has_native_controller": false,
                })
            })
            .collect(),
    )
}

fn modules() -> Value {
    const NAMED: &[&str] = &[
        "aggs-matrix-stats",
        "analysis-common",
        "geo",
        "ingest-common",
        "ingest-geoip",
        "ingest-user-agent",
        "lang-expression",
        "lang-mustache",
        "lang-painless",
        "mapper-extras",
        "opensearch-dashboards",
        "parent-join",
        "percolator",
        "rank-eval",
        "reindex",
        "repository-url",
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

/// `/_nodes/{*rest}` -- the node information, or one of the two reports that
/// live under the same prefix and are told apart by their last part.
pub async fn nodes_info_scoped(Path(rest): Path<String>, Query(p): Query<Params>) -> Response {
    if rest.split('/').next_back() == Some("usage") {
        return nodes_usage(Query(p)).await;
    }
    if matches!(rest.split('/').next_back(), Some("hot_threads" | "hotthreads")) {
        // the threads sampled are this process's: a node named in the path
        // that is not this one has none here to report
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() > 1 {
            let me = crate::cluster::identity();
            let here = parts[0].split(',').any(|n| {
                matches!(n, "_local" | "_all" | "*" | "_master" | "_cluster_manager")
                    || n == me.id.as_str()
                    || n == me.name
                    || (n.contains('*') && crate::store::glob_match(n, &me.name))
            });
            if !here {
                return ([("content-type", "text/plain; charset=UTF-8")], String::new())
                    .into_response();
            }
        }
        return super::hot_threads::hot_threads(&format!("/_nodes/{rest}"), &p).await;
    }
    nodes_info(Query(p)).await
}

/// A write under `/_nodes`: rereading the keystore is the only one.
pub async fn nodes_post(Path(rest): Path<String>, Query(p): Query<Params>) -> Response {
    if rest.split('/').next_back() == Some("reload_secure_settings") {
        return nodes_reload_secure_settings(Query(p)).await;
    }
    crate::api::err(
        axum::http::StatusCode::NOT_IMPLEMENTED,
        "not_implemented_exception",
        "not ported yet",
    )
}

pub async fn nodes_info(Query(p): Query<Params>) -> Response {
    let live = crate::cluster::current_state();
    let mut others = serde_json::Map::new();
    for (id, n) in &live.nodes {
        if *id == crate::cluster::identity().id {
            continue;
        }
        let ip =
            n.transport_address.rsplit_once(':').map(|(h, _)| h.to_string()).unwrap_or_default();
        others.insert(
            id.as_str().to_string(),
            json!({
                "name": n.name, "transport_address": n.transport_address, "host": ip, "ip": ip,
                "version": "3.9.0", "build_type": "tar", "build_hash": "boostsearch",
                "roles": n.roles, "attributes": n.attributes,
            }),
        );
    }
    let mut body = json!({
        "_nodes": {"total": live.nodes.len().max(1), "successful": live.nodes.len().max(1), "failed": 0},
        "cluster_name": crate::cluster::identity().cluster_name,
        "nodes": {crate::cluster::identity().id.as_str(): {
            "name": crate::cluster::identity().name, "transport_address": crate::cluster::identity().transport_address,
            "host": crate::cluster::identity().host, "ip": crate::cluster::identity().host, "version": "3.9.0",
            "build_type": "tar", "build_hash": "boostsearch", "roles": crate::cluster::identity().roles,
            "attributes": crate::cluster::identity().attributes,
            "os": {"refresh_interval_in_millis": 1000,
                   "available_processors": num_cpus(),
                   "allocated_processors": num_cpus()},
            "process": {"refresh_interval_in_millis": 1000, "id": std::process::id(),
                        "mlockall": false},
            "plugins": plugins(), "modules": modules(), "ingest": {"processors": crate::ingest::PROCESSOR_TYPES.iter().map(|t| json!({"type": t})).collect::<Vec<_>>()},
            "search_pipelines": {
                "request_processors": crate::search::pipeline::REQUEST_PROCESSORS.iter().map(|t| json!({"type": t})).collect::<Vec<_>>(),
                "response_processors": crate::search::pipeline::RESPONSE_PROCESSORS.iter().map(|t| json!({"type": t})).collect::<Vec<_>>(),
            },
            "thread_pool": {},
            // where the other nodes of the cluster reach this one
            "transport": {
                "bound_address": [crate::cluster::identity().transport_address.clone()],
                "publish_address": crate::cluster::identity().transport_address.clone(),
                "profiles": {},
            },
            // where a client -- or another cluster reindexing from this
            // one -- reaches this node
            "http": {
                "bound_address": [crate::api::bound_address()],
                "publish_address": crate::api::bound_address(),
                "max_content_length_in_bytes": crate::api::max_content_bytes(),
            },
        }},
    });
    if let Some(nodes) = body.get_mut("nodes").and_then(|n| n.as_object_mut()) {
        for (k, v) in others {
            nodes.insert(k, v);
        }
    }
    respond(&p, body)
}

/// `_nodes/usage` -- how much of the API each node has been asked for.
///
/// A node reports when it started counting and what it counted since; the
/// counts themselves are not kept here, so the lists are empty rather than
/// invented.
pub async fn nodes_usage(Query(p): Query<Params>) -> Response {
    let live = crate::cluster::current_state();
    let me = crate::cluster::identity();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut nodes = serde_json::Map::new();
    let listed: Vec<(String, String)> = if live.nodes.is_empty() {
        vec![(me.id.as_str().to_string(), me.name.clone())]
    } else {
        live.nodes.values().map(|n| (n.id.as_str().to_string(), n.name.clone())).collect()
    };
    for (id, _name) in listed {
        nodes.insert(
            id,
            json!({
                "timestamp": now,
                "since": now,
                "rest_actions": {},
                "aggregations": {},
            }),
        );
    }
    let total = nodes.len();
    crate::api::respond(
        &p,
        json!({
            "_nodes": {"total": total, "successful": total, "failed": 0},
            "cluster_name": me.cluster_name,
            "nodes": Value::Object(nodes),
        }),
    )
}

/// `_nodes/reload_secure_settings` -- read the keystore again.
///
/// There is no keystore to reread here, so every node answers that it did.
pub async fn nodes_reload_secure_settings(Query(p): Query<Params>) -> Response {
    let live = crate::cluster::current_state();
    let me = crate::cluster::identity();
    let mut nodes = serde_json::Map::new();
    let listed: Vec<(String, String)> = if live.nodes.is_empty() {
        vec![(me.id.as_str().to_string(), me.name.clone())]
    } else {
        live.nodes.values().map(|n| (n.id.as_str().to_string(), n.name.clone())).collect()
    };
    for (id, name) in listed {
        nodes.insert(id, json!({"name": name}));
    }
    let total = nodes.len();
    crate::api::respond(
        &p,
        json!({
            "_nodes": {"total": total, "successful": total, "failed": 0},
            "cluster_name": me.cluster_name,
            "nodes": Value::Object(nodes),
        }),
    )
}
