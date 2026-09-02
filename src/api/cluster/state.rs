//! What the cluster believes about itself, and where it has put things.

use super::*;

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
                "cluster_uuid": "_na_",
                "cluster_uuid_committed": false,
                "cluster_coordination": {
                    "term": 1,
                    "last_committed_config": ["node-0"],
                    "last_accepted_config": ["node-0"],
                    "voting_config_exclusions": [],
                },
                "templates": legacy_templates(&store),
                "indices": Value::Object(indices),
                "index-graveyard": {"tombstones": store.tombstones()},
                "index_template": {"index_template": composable_templates(&store)},
                "ingest": {"pipeline": ingest_pipelines(&store)},
            });
        }
        out["state"] = state;
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
                // whether the cluster's name has been agreed on, and by whom:
                // one node agrees with itself
                "cluster_uuid_committed": false,
                "templates": store.get_templates(),
                "indices": Value::Object(indices.clone()),
                "cluster_coordination": {
                    "term": 1,
                    "last_committed_config": ["node-0"],
                    "last_accepted_config": ["node-0"],
                    "voting_config_exclusions": store.voting_exclusions(),
                },
                // the indices that were deleted, so that a node coming back
                // with one of them knows it is gone
                "index-graveyard": {"tombstones": store.tombstones()},
                "index_template": {"index_template": composable_templates(&store)},
                "ingest": {"pipeline": ingest_pipelines(&store)},
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

/// The legacy templates alone: a composable one lives under its own key.
fn legacy_templates(store: &Store) -> Value {
    let all = store.get_templates();
    Value::Object(all.into_iter().filter(|(_, v)| v.get("__composable").is_none()).collect())
}

/// The composable templates, as the state lists them.
fn composable_templates(store: &Store) -> Value {
    let all = store.get_templates();
    Value::Object(
        all.into_iter()
            .filter_map(|(k, v)| {
                v.get("__composable").cloned().map(|mut c| {
                    if let Some(o) = c.as_object_mut() {
                        o.entry("composed_of").or_insert_with(|| json!([]));
                    }
                    (k, c)
                })
            })
            .collect(),
    )
}

/// The ingest pipelines, as the state lists them: each with its id.
fn ingest_pipelines(store: &Store) -> Value {
    let all = store.pipelines("ingest");
    Value::Array(
        all.into_iter()
            .map(|(id, def)| {
                let mut one = json!({"id": id});
                if let Some(o) = def.as_object() {
                    for (k, v) in o {
                        one[k] = v.clone();
                    }
                }
                one
            })
            .collect(),
    )
}
