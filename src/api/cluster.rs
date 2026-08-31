//! The cluster as a whole: its health, its state, its settings.

use super::*;

/// `_cluster/stats` -- the cluster in one page.
pub async fn cluster_stats(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    let names = store.names();
    let mut docs = 0u64;
    for n in &names {
        if let Some(st) = store.get(n) {
            docs += st.read().reader.searcher().num_docs();
        }
    }
    let replicated = names
        .iter()
        .filter_map(|n| store.get(n))
        .any(|st| st.read().numeric_setting("number_of_replicas").unwrap_or(0) > 0);
    respond(
        &p,
        json!({
            "_nodes": {"total": 1, "successful": 1, "failed": 0},
            "cluster_name": "boostsearch",
            "cluster_uuid": "_na_",
            "timestamp": 1_577_836_800_000u64,
            "status": if replicated { "yellow" } else { "green" },
            "indices": {
                "count": names.len(),
                "shards": {
                    "total": names.len(), "primaries": names.len(),
                    "replication": 0.0,
                    "index": {
                        "shards": {"min": 1, "max": 1, "avg": 1.0},
                        "primaries": {"min": 1, "max": 1, "avg": 1.0},
                        "replication": {"min": 0.0, "max": 0.0, "avg": 0.0},
                    },
                },
                "docs": {"count": docs, "deleted": 0},
                "store": {"size_in_bytes": 0, "reserved_in_bytes": 0},
                "fielddata": {"memory_size_in_bytes": 0, "evictions": 0},
                "query_cache": {
                    "memory_size_in_bytes": 0, "total_count": 0, "hit_count": 0,
                    "miss_count": 0, "cache_size": 0, "cache_count": 0, "evictions": 0,
                },
                "completion": {"size_in_bytes": 0},
                "segments": {
                    "count": 0, "memory_in_bytes": 0, "terms_memory_in_bytes": 0,
                    "stored_fields_memory_in_bytes": 0, "term_vectors_memory_in_bytes": 0,
                    "norms_memory_in_bytes": 0, "points_memory_in_bytes": 0,
                    "doc_values_memory_in_bytes": 0, "index_writer_memory_in_bytes": 0,
                    "version_map_memory_in_bytes": 0, "fixed_bit_set_memory_in_bytes": 0,
                    "max_unsafe_auto_id_timestamp": -1, "file_sizes": {},
                },
                "mappings": {"field_types": []},
                "analysis": {"char_filter_types": [], "tokenizer_types": [],
                             "filter_types": [], "analyzer_types": [],
                             "built_in_char_filters": [], "built_in_tokenizers": [],
                             "built_in_filters": [], "built_in_analyzers": []},
            },
            "nodes": {
                "count": {"total": 1, "cluster_manager": 1, "coordinating_only": 0,
                          "data": 1, "ingest": 1, "master": 1, "remote_cluster_client": 1,
                          "search": 0, "warm": 0},
                "versions": ["3.0.0"],
                "os": {
                    "available_processors": 1, "allocated_processors": 1,
                    "names": [], "pretty_names": [], "roles": [],
                    "mem": {
                        "total_in_bytes": 1_073_741_824u64,
                        "free_in_bytes": 536_870_912u64,
                        "used_in_bytes": 536_870_912u64,
                        "free_percent": 50, "used_percent": 50,
                    },
                },
                "process": {
                    "cpu": {"percent": 0},
                    "open_file_descriptors": {"min": 0, "max": 0, "avg": 0},
                },
                "jvm": {
                    "max_uptime_in_millis": 0, "versions": [],
                    "mem": {"heap_used_in_bytes": 0, "heap_max_in_bytes": 0},
                    "threads": 1,
                },
                "fs": {
                    "total_in_bytes": 2_147_483_648u64,
                    "free_in_bytes": 1_073_741_824u64,
                    "available_in_bytes": 1_073_741_824u64,
                },
                "plugins": [],
                "network_types": {
                    "transport_types": {"transport": 1},
                    "http_types": {"http": 1},
                },
                "discovery_types": {"single-node": 1},
                "packaging_types": [],
                "ingest": {"number_of_pipelines": 0, "processor_stats": {}},
            },
        }),
    )
}

/// `_remote/info` -- the clusters this one is connected to, of which there
/// are none.
pub async fn remote_info(Query(p): Query<Params>) -> Response {
    respond(&p, json!({}))
}

/// `_cluster/reroute` -- move shards about. There is one node, so there is
/// nowhere to move them, and the answer is the state as it stands.
pub async fn reroute(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let _ = parse_body(&body);
    let mut out = json!({"acknowledged": true, "explanations": []});
    // `metric` names which parts of the state to send back; without it the
    // state is left out entirely
    let metrics: Vec<String> = p
        .get("metric")
        .map(|m| m.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    let want = |name: &str| metrics.iter().any(|m| m == name || m == "_all");
    if !metrics.is_empty() && !metrics.iter().any(|m| m == "none") {
        let mut indices = serde_json::Map::new();
        for n in store.names() {
            let Some(st) = store.get(&n) else { continue };
            let g = st.read();
            indices.insert(
                g.name.clone(),
                json!({
                    "aliases": g.aliases.keys().cloned().collect::<Vec<_>>(),
                    "mappings": g.mapping.raw,
                    "settings": g.effective_settings(),
                    "state": if g.closed { "close" } else { "open" },
                }),
            );
        }
        let mut state = json!({
            "cluster_name": "boostsearch", "cluster_uuid": "_na_",
            "version": 1, "state_uuid": "_na_",
        });
        if want("master_node") {
            state["master_node"] = json!("node-0");
        }
        if want("cluster_manager_node") {
            state["cluster_manager_node"] = json!("node-0");
        }
        if want("nodes") {
            state["nodes"] = json!({"node-0": {
                "name": "boostsearch", "ephemeral_id": "_na_",
                "transport_address": "127.0.0.1:9300", "attributes": {}}});
        }
        if want("metadata") {
            state["metadata"] = json!({
                "cluster_uuid": "_na_", "templates": store.get_templates(),
                "indices": Value::Object(indices),
            });
        }
        out["state"] = state;
    }
    respond(&p, out)
}

/// `_cluster/pending_tasks` -- work the cluster manager has queued, of which
/// there is never any: everything here finishes before its request returns.
pub async fn pending_tasks(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"tasks": []}))
}

/// A single-node cluster is always green once it is up; the suite mostly uses
/// this endpoint as a barrier before it starts asserting.
pub async fn cluster_health(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i);
    // `expand_wildcards` decides whether a pattern reaches closed indices,
    // which are the ones that make the cluster less than green
    // health looks at every index by default, closed ones included: a closed
    // index still has shards, and they still count
    let states = p.get("expand_wildcards").map(|v| v.as_str()).unwrap_or("all");
    let want_open = states.split(',').any(|w| matches!(w.trim(), "open" | "all"));
    let want_closed = states.split(',').any(|w| matches!(w.trim(), "closed" | "all"));
    let names: Vec<String> = match expr.as_deref() {
        Some(e) if !e.is_empty() && e != "_all" => store
            .resolve(e)
            .into_iter()
            .filter(|n| {
                store
                    .get(n)
                    .map(|st| {
                        let closed = st.read().closed;
                        if !e.contains('*') {
                            true
                        } else if closed {
                            want_closed
                        } else {
                            want_open
                        }
                    })
                    .unwrap_or(false)
            })
            .collect(),
        _ => store.names(),
    };

    // one node can hold one copy of a shard, so an index asking for replicas
    // has some it will never get, and the cluster is yellow while it is in view
    let unassignable = names
        .iter()
        .filter_map(|n| store.get(n))
        .any(|st| st.read().numeric_setting("number_of_replicas").unwrap_or(0) > 0);
    let status = if unassignable { "yellow" } else { "green" };

    // a wait this engine cannot satisfy is answered as a timeout rather than
    // by waiting: nothing here is going to change while the request is held
    let satisfied = match p.get("wait_for_status").map(|v| v.as_str()) {
        Some("green") => status == "green",
        Some("yellow") => status != "red",
        _ => true,
    } && p
        .get("wait_for_nodes")
        .map(|v| {
            let want = v.trim_start_matches(['>', '<', '=']).parse::<i64>().unwrap_or(1);
            if v.starts_with('>') { 1 > want } else { 1 >= want }
        })
        .unwrap_or(true)
        // a wait for more active shards than the cluster has can never end
        && p.get("wait_for_active_shards")
            .map(|v| match v.as_str() {
                "all" => true,
                other => other.parse::<usize>().map(|n| n <= names.len()).unwrap_or(true),
            })
            .unwrap_or(true);

    // a shard is a shard whether or not the index it belongs to is open
    let shards_of =
        |name: &str| store.get(name).map(|st| st.read().shard_count() as usize).unwrap_or(1);
    let n: usize = names.iter().map(|name| shards_of(name)).sum();
    let mut out = json!({
        "cluster_name": "boostsearch", "status": status, "timed_out": !satisfied,
        "number_of_nodes": 1, "number_of_data_nodes": 1, "discovered_master": true,
        "discovered_cluster_manager": true,
        "active_primary_shards": n, "active_shards": n,
        "relocating_shards": 0, "initializing_shards": 0, "unassigned_shards": 0,
        "delayed_unassigned_shards": 0, "number_of_pending_tasks": 0,
        "number_of_in_flight_fetch": 0, "task_max_waiting_in_queue_millis": 0,
        "active_shards_percent_as_number": 100.0,
    });
    // `level` says how far down to report: the cluster, each index, or each
    // shard within them
    let level = p.get("level").map(|v| v.as_str()).unwrap_or("cluster");
    if level == "indices" || level == "shards" {
        let mut indices = serde_json::Map::new();
        for name in &names {
            let Some(st) = store.get(name) else { continue };
            let replicas = st.read().numeric_setting("number_of_replicas").unwrap_or(0);
            let shards = st.read().shard_count() as usize;
            let short = replicas > 0;
            let mut entry = json!({
                "status": if short { "yellow" } else { "green" },
                "number_of_shards": shards, "number_of_replicas": replicas,
                "active_primary_shards": shards, "active_shards": shards,
                "relocating_shards": 0, "initializing_shards": 0,
                "unassigned_shards": shards * replicas as usize,
            });
            if level == "shards" {
                let mut per = serde_json::Map::new();
                for shard in 0..shards {
                    per.insert(
                        shard.to_string(),
                        json!({
                            "status": if short { "yellow" } else { "green" },
                            "primary_active": true, "active_shards": 1,
                            "relocating_shards": 0, "initializing_shards": 0,
                            "unassigned_shards": replicas,
                        }),
                    );
                }
                entry["shards"] = Value::Object(per);
            }
            indices.insert(st.read().name.clone(), entry);
        }
        out["indices"] = Value::Object(indices);
    }
    if !satisfied {
        return (StatusCode::REQUEST_TIMEOUT, axum::Json(out)).into_response();
    }
    respond(&p, out)
}

/// `_cluster/allocation/explain` -- why a shard sits where it does.
///
/// Every shard is started on the one node, so the only honest explanation is
/// that it is where it belongs and has nowhere else to go.
pub async fn allocation_explain(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    // without a shard named, the question is which unassigned shard needs
    // explaining: an index asking for more replicas than there are nodes has
    // some, and otherwise there are none to explain
    let unassigned = store.names().into_iter().find(|n| {
        store
            .get(n)
            .map(|st| st.read().numeric_setting("number_of_replicas").unwrap_or(0) > 0)
            .unwrap_or(false)
    });
    let named = body.get("index").and_then(|v| v.as_str()).map(|s| s.to_string());
    let picked = named.is_none();
    let Some(index) = named.or(unassigned) else {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "unable to find any unassigned shards to explain [ClusterAllocationExplainRequest[useAnyUnassignedShard=true]]",
        );
    };
    let index = index.as_str();
    if store.resolve(index).is_empty() {
        return no_such_index(index);
    }
    let shard = body.get("shard").and_then(|v| v.as_i64()).unwrap_or(0);
    // a request that names nothing is asking about whichever shard is waiting
    // for somewhere to live, and that is a replica, not a primary
    let primary = body.get("primary").and_then(|v| v.as_bool()).unwrap_or(!picked);
    let mut out = json!({
        "index": index,
        "shard": shard,
        "primary": primary,
    });
    if picked {
        out["current_state"] = json!("unassigned");
        out["unassigned_info"] = json!({
            "reason": "INDEX_CREATED",
            "at": IdxState::now_iso(),
            "last_allocation_status": "no_attempt",
        });
        out["can_allocate"] = json!("no");
        out["allocate_explanation"] =
            json!("cannot allocate because allocation is not permitted to any of the nodes");
    } else {
        out["current_state"] = json!("started");
        out["current_node"] = json!({
            "id": "node-0", "name": "boostsearch",
            "transport_address": "127.0.0.1:9300", "weight_ranking": 1,
        });
        out["can_remain_on_current_node"] = json!("yes");
        out["can_rebalance_cluster"] = json!("yes");
        out["can_rebalance_to_other_node"] = json!("no");
        out["rebalance_explanation"] = json!(
            "cannot rebalance as no target node exists that can both allocate this shard \
             and improve the cluster balance"
        );
    }
    out["node_allocation_decisions"] = json!([]);
    // `include_disk_info` asks for what the cluster knows about the disks
    if flag(&p, "include_disk_info") {
        out["cluster_info"] = json!({
            "nodes": {
                "node-0": {
                    "node_name": "boostsearch",
                    "least_available": {
                        "path": "/", "total_bytes": 2_147_483_648u64,
                        "used_bytes": 1_073_741_824u64,
                        "free_bytes": 1_073_741_824u64, "free_disk_percent": 50.0,
                        "used_disk_percent": 50.0,
                    },
                    "most_available": {
                        "path": "/", "total_bytes": 2_147_483_648u64,
                        "used_bytes": 1_073_741_824u64,
                        "free_bytes": 1_073_741_824u64, "free_disk_percent": 50.0,
                        "used_disk_percent": 50.0,
                    },
                }
            },
            "shard_sizes": {},
            "shard_paths": {},
        });
    }
    respond(&p, out)
}

/// `_cluster/voting_config_exclusions` -- nodes kept out of the vote that
/// elects a cluster manager.
///
/// One node has no election to hold, but the exclusions are still recorded
/// and reported, since a caller draining a node watches this list to know the
/// exclusion took.
pub async fn post_voting_config_exclusions(
    State(store): State<Store>,
    Query(p): Query<Params>,
) -> Response {
    let ids = p.get("node_ids").filter(|v| !v.is_empty());
    let names = p.get("node_names").or_else(|| p.get("node_name")).filter(|v| !v.is_empty());
    let entries: Vec<Value> = match (ids, names) {
        (Some(ids), None) => {
            ids.split(',').map(|n| json!({"node_id": n.trim(), "node_name": "_absent_"})).collect()
        }
        (None, Some(names)) => names
            .split(',')
            .map(|n| json!({"node_id": "_absent_", "node_name": n.trim()}))
            .collect(),
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                "Please set node identifiers correctly. One and only one of [node_name], \
                 [node_names] and [node_ids] has to be set",
            );
        }
    };
    store.add_voting_exclusions(entries);
    (StatusCode::OK, axum::Json(json!({}))).into_response()
}

pub async fn delete_voting_config_exclusions(State(store): State<Store>) -> Response {
    store.clear_voting_exclusions();
    (StatusCode::OK, axum::Json(json!({}))).into_response()
}

/// `/_cluster/state/<metrics>` and `/_cluster/state/<metrics>/<indices>`:
/// the first path part names which sections to return, the second which
/// indices the metadata should describe.
pub async fn cluster_state_filtered(
    State(store): State<Store>,
    Path(rest): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let mut parts = rest.splitn(2, '/');
    let metrics = parts.next().unwrap_or("_all").to_string();
    let indices = parts.next().map(|s| s.to_string());
    cluster_state_inner(&store, &p, Some(&metrics), indices.as_deref())
}

pub async fn cluster_state(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    cluster_state_inner(&store, &p, None, None)
}

pub(crate) fn cluster_state_inner(
    store: &Store,
    p: &Params,
    metrics: Option<&str>,
    index_expr: Option<&str>,
) -> Response {
    let all = metrics.map(|m| m == "_all").unwrap_or(true);
    let wanted: Vec<&str> = metrics.map(|m| m.split(',').collect()).unwrap_or_default();
    let want = |name: &str| all || wanted.contains(&name);

    // which indices the metadata should describe; without an expression it is
    // every index there is
    let names = match index_expr {
        Some(expr) if expr != "_all" => {
            // `expand_wildcards` names the states a pattern reaches, and
            // naming only one of them leaves the other out
            let states = p.get("expand_wildcards").map(|v| v.as_str()).unwrap_or("open,closed");
            let want_open = states.split(',').any(|w| matches!(w.trim(), "open" | "all"));
            let want_closed = states.split(',').any(|w| matches!(w.trim(), "closed" | "all"));
            let found: Vec<String> = store
                .resolve(expr)
                .into_iter()
                .filter(|n| {
                    store
                        .get(n)
                        .map(|st| if st.read().closed { want_closed } else { want_open })
                        .unwrap_or(false)
                })
                .collect();
            for part in expr.split(',').map(|n| n.trim()).filter(|n| !n.contains('*')) {
                if !found.iter().any(|n| n == part) && !ignore_unavailable(p) {
                    return no_such_index(part);
                }
            }
            // a pattern reaching nothing is an error only when the caller said so
            let allow_none = p.get("allow_no_indices").map(|v| v != "false").unwrap_or(true);
            if found.is_empty() && !allow_none {
                return no_such_index(expr);
            }
            found
        }
        _ => store.names(),
    };

    let mut indices = serde_json::Map::new();
    for n in names {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        // the space a shard count can be split into later: the power-of-two
        // multiple of the shards the index was given
        let mut routing_shards = g.numeric_setting("number_of_shards").unwrap_or(1).max(1);
        while routing_shards * 2 <= 1024 {
            routing_shards *= 2;
        }
        indices.insert(
            g.name.clone(),
            json!({
                "aliases": g.aliases.keys().cloned().collect::<Vec<_>>(),
                "mappings": g.mapping.raw,
                "settings": g.effective_settings(),
                "state": if g.closed { "close" } else { "open" },
                "routing_num_shards": routing_shards,
            }),
        );
    }

    let mut out = serde_json::Map::new();
    out.insert("cluster_name".into(), json!("boostsearch"));
    out.insert("cluster_uuid".into(), json!("_na_"));
    if want("version") || all {
        out.insert("version".into(), json!(1));
        out.insert("state_uuid".into(), json!("_na_"));
    }
    // the two names are the old and new spellings of one thing, but a
    // request naming one does not ask for the other
    if want("master_node") {
        out.insert("master_node".into(), json!("node-0"));
    }
    if want("cluster_manager_node") {
        out.insert("cluster_manager_node".into(), json!("node-0"));
    }
    if want("nodes") {
        out.insert(
            "nodes".into(),
            json!({"node-0": {
            "name": "boostsearch", "ephemeral_id": "_na_",
            "transport_address": "127.0.0.1:9300", "attributes": {}}}),
        );
    }
    if want("metadata") {
        out.insert(
            "metadata".into(),
            json!({
                "cluster_uuid": "_na_",
                "templates": store.get_templates(),
                "indices": Value::Object(indices.clone()),
                "cluster_coordination": {
                    "voting_config_exclusions": store.voting_exclusions(),
                },
            }),
        );
    }
    if want("blocks") {
        // an index held still, or closed, is one the cluster is blocking
        let mut per_index = serde_json::Map::new();
        for name in indices.keys() {
            let Some(st) = store.get(name) else { continue };
            let g = st.read();
            let mut held = serde_json::Map::new();
            if g.closed {
                held.insert(
                    "4".into(),
                    json!({
                        "description": "index closed", "retryable": false,
                        "levels": ["read", "write"],
                    }),
                );
            }
            if g.setting("blocks.write").as_deref() == Some("true") {
                held.insert(
                    "8".into(),
                    json!({
                        "description": "index write (api)", "retryable": false,
                        "levels": ["write"],
                    }),
                );
            }
            // a read-only index refuses writes and metadata changes both
            if g.setting("blocks.read_only").as_deref() == Some("true") {
                held.insert(
                    "5".into(),
                    json!({
                        "description": "index read-only (api)", "retryable": false,
                        "levels": ["write", "metadata_write"],
                    }),
                );
            }
            if g.setting("blocks.metadata").as_deref() == Some("true") {
                held.insert(
                    "9".into(),
                    json!({
                        "description": "index metadata (api)", "retryable": false,
                        "levels": ["metadata_write", "metadata_read"],
                    }),
                );
            }
            if g.setting("blocks.read").as_deref() == Some("true") {
                held.insert(
                    "7".into(),
                    json!({
                        "description": "index read (api)", "retryable": false,
                        "levels": ["read"],
                    }),
                );
            }
            if !held.is_empty() {
                per_index.insert(name.clone(), Value::Object(held));
            }
        }
        out.insert(
            "blocks".into(),
            if per_index.is_empty() {
                json!({})
            } else {
                json!({"indices": Value::Object(per_index)})
            },
        );
    }
    if want("routing_table") {
        let mut tables = serde_json::Map::new();
        for name in indices.keys() {
            tables.insert(
                name.clone(),
                json!({"shards": {"0": [{
                    "state": "STARTED", "primary": true, "node": "node-0",
                    "relocating_node": Value::Null, "shard": 0, "index": name,
                }]}}),
            );
        }
        out.insert("routing_table".into(), json!({"indices": Value::Object(tables)}));
    }
    if want("routing_nodes") {
        let shards: Vec<Value> = indices
            .keys()
            .map(|name| {
                json!({
                    "state": "STARTED", "primary": true, "node": "node-0",
                    "relocating_node": Value::Null, "shard": 0, "index": name,
                })
            })
            .collect();
        out.insert("routing_nodes".into(), json!({"unassigned": [], "nodes": {"node-0": shards}}));
    }
    respond(p, Value::Object(out))
}

/// Walk a settings body into dotted keys with text values, whichever way the
/// caller wrote it.
pub(crate) fn flatten_cluster_settings(
    node: &Value,
    prefix: &str,
    out: &mut serde_json::Map<String, Value>,
) {
    match node {
        Value::Object(o) => {
            for (k, v) in o {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten_cluster_settings(v, &key, out);
            }
        }
        Value::Null => {
            out.insert(prefix.to_string(), Value::Null);
        }
        Value::String(s) => {
            out.insert(prefix.to_string(), json!(s));
        }
        other => {
            out.insert(prefix.to_string(), json!(other.to_string()));
        }
    }
}

/// A setting whose value the cluster refuses rather than stores.
pub(crate) fn check_cluster_setting(key: &str, value: &Value) -> Option<Response> {
    // a null is a removal, not a value, and nothing about it can be wrong
    if value.is_null() {
        return None;
    }
    // a cancellation rate or ratio of zero would cancel nothing, which is not
    // a setting so much as a way of turning the feature off by halves
    if key.starts_with("search_backpressure.") && key.contains("cancellation_") {
        let n =
            value.as_f64().or_else(|| value.as_str().and_then(|s| s.parse().ok())).unwrap_or(1.0);
        if n <= 0.0 {
            return Some(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("{key} must be > 0"),
            ));
        }
    }
    if key == "search_backpressure.mode" {
        let v = value.as_str().unwrap_or("");
        if !matches!(v, "monitor_only" | "enforced" | "disabled") {
            return Some(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("Invalid SearchBackpressureMode: {v}"),
            ));
        }
    }
    None
}

pub async fn cluster_settings_get(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    let raw = store.cluster_settings();
    let flat = p.get("flat_settings").map(|v| v == "true").unwrap_or(false);
    let view = |scope: &str| match raw.get(scope) {
        Some(v) if !flat => nest_settings(v),
        Some(v) => v.clone(),
        None => json!({}),
    };
    // `include_defaults` asks for the settings nobody set, which here is what
    // the node was started with
    let mut defaults = json!({});
    if flag(&p, "include_defaults") {
        for (k, v) in node_attrs() {
            defaults[format!("node.attr.{k}")] = json!(v);
        }
        if !flat {
            defaults = nest_settings(&defaults);
        }
    }
    respond(
        &p,
        json!({
            "persistent": view("persistent"),
            "transient": view("transient"),
            "defaults": defaults,
        }),
    )
}

pub async fn cluster_settings_put(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let mut body: Value = parse_body(&body).unwrap_or(json!({}));
    // settings arrive dotted or nested and are held one way: dotted, with the
    // value as text, which is how they are reported back
    for scope in ["persistent", "transient"] {
        let Some(o) = body.get(scope).and_then(|v| v.as_object()).cloned() else { continue };
        let mut flat = serde_json::Map::new();
        flatten_cluster_settings(&Value::Object(o), "", &mut flat);
        for (k, v) in &flat {
            if let Some(r) = check_cluster_setting(k, v) {
                return r;
            }
        }
        body[scope] = Value::Object(flat);
    }
    store.merge_cluster_settings(&body);
    // the answer is shaped the way the request asked to see settings
    let flat = p.get("flat_settings").map(|v| v == "true").unwrap_or(false);
    let echo = |scope: &str| match body.get(scope) {
        Some(v) if flat => {
            // a key set to null was a removal, and is not in the result
            let mut o = v.as_object().cloned().unwrap_or_default();
            o.retain(|_, val| !val.is_null());
            Value::Object(o)
        }
        Some(v) => nest_settings(v),
        None => json!({}),
    };
    respond(
        &p,
        json!({
            "acknowledged": true,
            "persistent": echo("persistent"),
            "transient": echo("transient"),
        }),
    )
}
