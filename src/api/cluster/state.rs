//! What the cluster believes about itself, and where it has put things.

use super::*;

/// `_cluster/reroute` -- move shards about, by asking the cluster manager
/// to; the answer is the state as it comes out.
pub async fn reroute(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let commands = body.get("commands").cloned().unwrap_or(json!([]));
    if !commands.is_array() {
        return err(StatusCode::BAD_REQUEST, "parsing_exception", "[commands] must be an array");
    }
    let dry_run = flag(&p, "dry_run");
    let explain = flag(&p, "explain");
    let retry_failed = flag(&p, "retry_failed");
    let live = crate::cluster::current_state();
    let (Some(manager), Some(rt)) = (live.cluster_manager.clone(), crate::cluster::runtime())
    else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "master_not_discovered_exception", "null");
    };
    let ask = json!({"commands": commands, "dry_run": dry_run, "explain": explain, "retry_failed": retry_failed});
    let answer = rt
        .call(
            &manager,
            crate::cluster::coordinator::REROUTE,
            serde_json::to_vec(&ask).unwrap_or_default(),
            std::time::Duration::from_secs(30),
        )
        .await;
    let Some(answer) = answer else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "master_not_discovered_exception", "null");
    };
    if answer.kind == crate::cluster::transport::Kind::Error {
        let v: Value = serde_json::from_slice(&answer.body).unwrap_or(Value::Null);
        let reason =
            v.get("reason").and_then(|r| r.as_str()).unwrap_or("reroute failed").to_string();
        return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", &reason);
    }
    let v: Value = serde_json::from_slice(&answer.body).unwrap_or(Value::Null);
    let Some(state) = v.get("state").and_then(|s| {
        serde_json::from_value::<crate::cluster::state::ClusterState>(s.clone()).ok()
    }) else {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "exception",
            "the manager answered with no state",
        );
    };
    // the state as `_cluster/state` shows it, without the metadata unless asked
    let metrics: String =
        p.get("metric").filter(|m| !m.is_empty()).cloned().unwrap_or_else(|| {
            "version,master_node,nodes,routing_table,routing_nodes,blocks".into()
        });
    let mut out = json!({"acknowledged": true});
    if !metrics.split(',').any(|m| m.trim() == "none") {
        let mut inner = p.clone();
        inner.remove("filter_path");
        let rendered = crate::cluster::with_state_override(state, || {
            cluster_state_value(&store, &inner, Some(&metrics), None)
        });
        match rendered {
            Ok(st) => out["state"] = st,
            Err(r) => return r,
        }
    }
    if explain {
        out["explanations"] = v.get("explanations").cloned().unwrap_or(json!([]));
    }
    respond(&p, out)
}

/// `_cluster/allocation/explain` -- why a shard sits where it does: the
/// deciders asked about the copy, in the plugin's words.
pub async fn allocation_explain(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    use crate::cluster::allocation::{ClusterSettings, Context, explain};
    use crate::cluster::state::ShardState;
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let live = crate::cluster::current_state();
    let named = body.get("index").and_then(|v| v.as_str()).map(|s| s.to_string());
    let copy = match &named {
        Some(index) => {
            if !live.routing.indices.contains_key(index) && store.resolve(index).is_empty() {
                return no_such_index(index);
            }
            let shard = body.get("shard").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let primary = body.get("primary").and_then(|v| v.as_bool()).unwrap_or(true);
            let found = live
                .routing
                .shards_of(index)
                .find(|c| c.shard == shard && c.primary == primary)
                .cloned();
            match found {
                Some(c) => c,
                None => {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        &format!("No shard was found for [{index}][{shard}]"),
                    );
                }
            }
        }
        None => {
            // which unassigned shard needs explaining: the first there is
            match live.routing.all().find(|c| c.state == ShardState::Unassigned).cloned() {
                Some(c) => c,
                None => {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        "unable to find any unassigned shards to explain [ClusterAllocationExplainRequest[useAnyUnassignedShard=true]]",
                    );
                }
            }
        }
    };
    let cluster = ClusterSettings::from_value(&live.cluster_settings);
    // a primary is at home where it is
    let home: std::collections::BTreeMap<String, crate::cluster::NodeId> = live
        .routing
        .indices
        .iter()
        .filter_map(|(n, shards)| {
            shards
                .values()
                .flatten()
                .find(|c| c.primary && c.node.is_some())
                .map(|c| (n.clone(), c.node.clone().unwrap()))
        })
        .collect();
    let held = std::collections::BTreeMap::new();
    let ctx = Context {
        nodes: &live.nodes,
        indices: &live.indices,
        cluster: &cluster,
        primary_home: &home,
        held: &held,
        now: crate::cluster::clock().wall(),
    };
    let mut out = explain(&ctx, &live, &copy, flag(&p, "include_yes_decisions"));
    // `include_disk_info` asks for what the cluster knows about the disks
    if flag(&p, "include_disk_info") {
        let mut nodes = serde_json::Map::new();
        for n in live.nodes.values() {
            nodes.insert(
                n.id.as_str().to_string(),
                json!({
                    "node_name": n.name,
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
                }),
            );
        }
        out["cluster_info"] = json!({"nodes": nodes, "shard_sizes": {}, "shard_paths": {}});
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
    match cluster_state_value(store, p, metrics, index_expr) {
        Ok(out) => respond(p, out),
        Err(r) => r,
    }
}

pub(crate) fn cluster_state_value(
    store: &Store,
    p: &Params,
    metrics: Option<&str>,
    index_expr: Option<&str>,
) -> Result<Value, Response> {
    let all = metrics.map(|m| m == "_all").unwrap_or(true);
    let wanted: Vec<&str> = metrics.map(|m| m.split(',').collect()).unwrap_or_default();
    let want = |name: &str| all || wanted.contains(&name);

    // which indices the metadata should describe; without an expression it is
    // every index there is
    let live = crate::cluster::current_state();
    let names = match index_expr {
        Some(expr) if expr != "_all" => {
            // `expand_wildcards` names the states a pattern reaches, and
            // naming only one of them leaves the other out
            let states = p.get("expand_wildcards").map(|v| v.as_str()).unwrap_or("open,closed");
            let want_open = states.split(',').any(|w| matches!(w.trim(), "open" | "all"));
            let want_closed = states.split(',').any(|w| matches!(w.trim(), "closed" | "all"));
            let mut found: Vec<String> = store
                .resolve(expr)
                .into_iter()
                .filter(|n| {
                    store
                        .get(n)
                        .map(|st| if st.read().closed { want_closed } else { want_open })
                        .unwrap_or(false)
                })
                .collect();
            // an index the cluster manager published that this node does not
            // hold answers by name too (a follower's view)
            for (name, m) in &live.indices {
                let closed = m.state == "close";
                let wanted = if closed { want_closed } else { want_open };
                let named = expr.split(',').map(|n| n.trim()).any(|part| {
                    part == name.as_str()
                        || (part.contains('*')
                            && crate::store::wildcard_to_regex(part).is_match(name))
                });
                if wanted && named && !found.iter().any(|f| f == name) {
                    found.push(name.clone());
                }
            }
            for part in expr.split(',').map(|n| n.trim()).filter(|n| !n.contains('*')) {
                if !found.iter().any(|n| n == part) && !ignore_unavailable(p) {
                    return Err(no_such_index(part));
                }
            }
            // a pattern reaching nothing is an error only when the caller said so
            let allow_none = p.get("allow_no_indices").map(|v| v != "false").unwrap_or(true);
            if found.is_empty() && !allow_none {
                return Err(no_such_index(expr));
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
        indices.insert(g.name.clone(), {
            let mut entry = json!({
                "aliases": g.aliases.keys().cloned().collect::<Vec<_>>(),
                "mappings": g.mapping.raw,
                "settings": g.effective_settings(),
                "state": if g.closed { "close" } else { "open" },
                "version": 1,
                "mapping_version": 1,
                "settings_version": 1,
                "aliases_version": 1,
                "routing_num_shards": routing_shards,
                "rollover_info": {},
                "system": false,
            });
            if let Some(m) = crate::cluster::current_state().indices.get(&g.name) {
                let published = m.to_json();
                for k in [
                    "primary_terms",
                    "in_sync_allocations",
                    "version",
                    "mapping_version",
                    "settings_version",
                    "aliases_version",
                ] {
                    if let Some(v) = published.get(k) {
                        entry[k] = v.clone();
                    }
                }
            } else {
                let shards = g.shard_count().max(1);
                entry["primary_terms"] =
                    Value::Object((0..shards).map(|i| (i.to_string(), json!(1))).collect());
                entry["in_sync_allocations"] =
                    Value::Object((0..shards).map(|i| (i.to_string(), json!([]))).collect());
            }
            entry
        });
    }
    // an index published but not held here (a follower's view): as published
    for (name, m) in &live.indices {
        if !indices.contains_key(name) {
            indices.insert(name.clone(), m.to_json());
        }
    }

    let mut out = serde_json::Map::new();
    let manager =
        live.cluster_manager.clone().unwrap_or_else(|| crate::cluster::identity().id.clone());
    out.insert("cluster_name".into(), json!(crate::cluster::identity().cluster_name));
    out.insert("cluster_uuid".into(), json!(live.cluster_uuid));
    if want("version") || all {
        out.insert("version".into(), json!(live.version));
        out.insert("state_uuid".into(), json!(live.state_uuid));
        out.insert("version".into(), json!(live.version));
    }
    // the two names are the old and new spellings of one thing, but a
    // request naming one does not ask for the other
    if want("master_node") {
        out.insert("master_node".into(), json!(manager.as_str()));
    }
    if want("cluster_manager_node") {
        out.insert("cluster_manager_node".into(), json!(manager.as_str()));
    }
    if want("nodes") {
        out.insert("nodes".into(), live.node_json());
    }
    if want("metadata") {
        let graveyard = {
            let live = crate::cluster::current_state();
            if live.graveyard.is_empty() {
                store.tombstones()
            } else {
                Value::Array(live.graveyard.clone())
            }
        };
        out.insert(
            "metadata".into(),
            json!({
                "cluster_uuid": live.cluster_uuid,
                // whether the cluster's name has been agreed on, and by whom:
                // one node agrees with itself
                "cluster_uuid_committed": true,
                "templates": store.get_templates(),
                "indices": Value::Object(indices.clone()),
                "cluster_coordination": {
                    "term": live.term.max(1),
                    "last_committed_config": live.last_committed_config.iter().map(|n| n.as_str()).collect::<Vec<_>>(),
                    "last_accepted_config": live.last_accepted_config.iter().map(|n| n.as_str()).collect::<Vec<_>>(),
                    "voting_config_exclusions": store.voting_exclusions(),
                },
                // the indices that were deleted, so that a node coming back
                // with one of them knows it is gone
                "index-graveyard": {"tombstones": graveyard},
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
    // the shard map the cluster manager published; an index the store
    // holds but the state has not yet seen is placed the way it will be
    let placed = live.routing.indices.keys().cloned().collect::<std::collections::HashSet<_>>();
    let mut routing = live.routing.clone();
    for name in indices.keys() {
        if !placed.contains(name) {
            let shards =
                store.get(name).map(|st| st.read().shard_count().max(1) as u32).unwrap_or(1);
            let mut copies = std::collections::BTreeMap::new();
            for shard in 0..shards {
                copies.insert(
                    shard,
                    vec![crate::cluster::state::ShardRouting {
                        index: name.clone(),
                        shard,
                        primary: true,
                        state: crate::cluster::state::ShardState::Started,
                        node: Some(manager.clone()),
                        relocating_node: None,
                        allocation_id: None,
                        unassigned: None,
                    }],
                );
            }
            routing.indices.insert(name.clone(), copies);
        }
    }
    routing.indices.retain(|n, _| indices.contains_key(n));
    if want("routing_table") {
        out.insert("routing_table".into(), routing.to_json());
    }
    if want("routing_nodes") {
        let (by_node, unassigned) = routing.by_node();
        let mut nodes = serde_json::Map::new();
        for (n, copies) in by_node {
            nodes.insert(
                n.as_str().to_string(),
                Value::Array(copies.iter().map(|c| c.to_json()).collect()),
            );
        }
        if !nodes.contains_key(manager.as_str()) {
            nodes.insert(manager.as_str().to_string(), json!([]));
        }
        out.insert(
            "routing_nodes".into(),
            json!({"unassigned": unassigned.iter().map(|c| c.to_json()).collect::<Vec<_>>(), "nodes": nodes}),
        );
    }
    Ok(Value::Object(out))
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
