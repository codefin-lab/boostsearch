//! Settings that belong to the cluster rather than to an index.

use super::*;

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
    // an exclusion names a node; the ones that name nobody are recorded as they are
    let live = crate::cluster::current_state();
    let entries: Vec<Value> = entries
        .into_iter()
        .map(|e| {
            let id = e["node_id"].as_str().unwrap_or("_absent_");
            let name = e["node_name"].as_str().unwrap_or("_absent_");
            match live.nodes.values().find(|n| n.id.as_str() == id || n.name == name) {
                Some(n) => json!({"node_id": n.id.as_str(), "node_name": n.name}),
                None => e,
            }
        })
        .collect();
    store.add_voting_exclusions(entries.clone());
    // the reply waits for the exclusions to take effect: the excluded nodes
    // out of the committed voting configuration
    let timeout = p.get("timeout").and_then(|t| parse_time_ms(t)).unwrap_or(30_000);
    let started = std::time::Instant::now();
    loop {
        let live = crate::cluster::current_state();
        let pending: Vec<&Value> = entries
            .iter()
            .filter(|e| {
                live.last_committed_config
                    .iter()
                    .any(|n| n.as_str() == e["node_id"].as_str().unwrap_or(""))
            })
            .collect();
        if pending.is_empty() {
            return (StatusCode::OK, axum::Json(json!({}))).into_response();
        }
        if started.elapsed().as_millis() as u64 >= timeout {
            let list: Vec<String> = pending
                .iter()
                .map(|e| {
                    format!(
                        "{{{}}}{{{}}}",
                        e["node_name"].as_str().unwrap_or(""),
                        e["node_id"].as_str().unwrap_or("")
                    )
                })
                .collect();
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "timeout_exception",
                format!(
                    "timed out waiting for voting config exclusions [{}] to take effect",
                    list.join(", ")
                ),
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

pub async fn delete_voting_config_exclusions(
    State(store): State<Store>,
    Query(p): Query<Params>,
) -> Response {
    // `wait_for_removal` waits for the excluded nodes to have left the cluster
    let wait = p.get("wait_for_removal").map(|v| v != "false").unwrap_or(true);
    let timeout = p.get("timeout").and_then(|t| parse_time_ms(t)).unwrap_or(30_000);
    let excluded = store.voting_exclusions();
    let started = std::time::Instant::now();
    // `wait` does not change: it says whether to wait at all, and the loop
    // ends when the nodes have gone or the timeout is up
    if wait {
        loop {
            let live = crate::cluster::current_state();
            let still: Vec<String> = excluded
                .iter()
                .filter(|e| {
                    live.nodes.contains_key(&crate::cluster::NodeId(
                        e["node_id"].as_str().unwrap_or("").to_string(),
                    ))
                })
                .map(|e| {
                    format!(
                        "{{{}}}{{{}}}",
                        e["node_name"].as_str().unwrap_or(""),
                        e["node_id"].as_str().unwrap_or("")
                    )
                })
                .collect();
            if still.is_empty() {
                break;
            }
            if started.elapsed().as_millis() as u64 >= timeout {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "timeout_exception",
                    format!(
                        "timed out waiting for removal of nodes; if nodes should not be removed, set waitForRemoval to false. [{}]",
                        still.join(", ")
                    ),
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    store.clear_voting_exclusions();
    (StatusCode::OK, axum::Json(json!({}))).into_response()
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
/// The families of settings a cluster takes. A setting is named for the part
/// of the server it belongs to, and one whose family nothing here answers for
/// is a name OpenSearch does not know either.
const SETTING_FAMILIES: &[&str] = &[
    "action",
    "admission_control",
    "bootstrap",
    "cluster",
    "cluster_manager",
    "compatibility",
    "discovery",
    "gateway",
    "http",
    "indices",
    "ingest",
    "knn",
    "logger",
    "monitor",
    "network",
    "no_master_block",
    "node",
    "opendistro",
    "opensearch",
    "path",
    "persistent_tasks",
    "plugins",
    "processors",
    "remote_store",
    "repositories",
    "script",
    "search",
    "search_backpressure",
    "segrep",
    "shard_indexing_pressure",
    "snapshot",
    "task_resource_consumers",
    "telemetry",
    "thread_pool",
    "transport",
    "wlm",
];

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

/// Cluster settings and the value each has when nobody set it, as the
/// reference lists them under `include_defaults`.
const COMMON_DEFAULTS: &[(&str, &str)] = &[
    ("action.auto_create_index", "true"),
    ("action.destructive_requires_name", "false"),
    ("action.search.shard_count.limit", "9223372036854775807"),
    ("bootstrap.memory_lock", "false"),
    ("cluster.blocks.read_only", "false"),
    ("cluster.blocks.read_only_allow_delete", "false"),
    ("cluster.indices.close.enable", "true"),
    ("cluster.info.update.interval", "30s"),
    ("cluster.max_shards_per_node", "1000"),
    ("cluster.max_voting_config_exclusions", "10"),
    ("cluster.no_cluster_manager_block", "metadata_write"),
    ("cluster.persistent_tasks.allocation.enable", "all"),
    ("cluster.remote.connect", "true"),
    ("cluster.remote.initial_connect_timeout", "30s"),
    ("cluster.routing.allocation.allow_rebalance", "indices_all_active"),
    ("cluster.routing.allocation.balance.index", "0.55"),
    ("cluster.routing.allocation.balance.shard", "0.45"),
    ("cluster.routing.allocation.balance.threshold", "1.0"),
    ("cluster.routing.allocation.cluster_concurrent_rebalance", "2"),
    ("cluster.routing.allocation.disk.reroute_interval", "60s"),
    ("cluster.routing.allocation.disk.threshold_enabled", "true"),
    ("cluster.routing.allocation.disk.watermark.flood_stage", "95%"),
    ("cluster.routing.allocation.disk.watermark.high", "90%"),
    ("cluster.routing.allocation.disk.watermark.low", "85%"),
    ("cluster.routing.allocation.enable", "all"),
    ("cluster.routing.allocation.node_concurrent_incoming_recoveries", "2"),
    ("cluster.routing.allocation.node_concurrent_outgoing_recoveries", "2"),
    ("cluster.routing.allocation.node_concurrent_recoveries", "2"),
    ("cluster.routing.allocation.node_initial_primaries_recoveries", "4"),
    ("cluster.routing.allocation.same_shard.host", "false"),
    ("cluster.routing.allocation.total_shards_per_node", "-1"),
    ("cluster.routing.rebalance.enable", "all"),
    ("cluster.routing.use_adaptive_replica_selection", "true"),
    ("gateway.expected_data_nodes", "-1"),
    ("http.compression", "true"),
    ("http.cors.enabled", "false"),
    ("http.max_content_length", "100mb"),
    ("http.max_header_size", "16384b"),
    ("http.max_initial_line_length", "4096b"),
    ("indices.breaker.fielddata.limit", "40%"),
    ("indices.breaker.request.limit", "60%"),
    ("indices.breaker.total.limit", "95%"),
    ("indices.breaker.total.use_real_memory", "true"),
    ("indices.fielddata.cache.size", "35.0%"),
    ("indices.id_field_data.enabled", "true"),
    ("indices.memory.index_buffer_size", "10%"),
    ("indices.queries.cache.size", "10%"),
    ("indices.query.bool.max_clause_count", "1024"),
    ("indices.recovery.max_bytes_per_sec", "41943040b"),
    ("indices.recovery.max_concurrent_file_chunks", "2"),
    ("indices.requests.cache.size", "1%"),
    ("plugins.index_state_management.job_interval", "5"),
    ("script.max_compilations_rate", "use-context"),
    ("script.max_size_in_bytes", "65535"),
    ("search.allow_expensive_queries", "true"),
    ("search.concurrent_segment_search.mode", "auto"),
    ("search.default_allow_partial_results", "true"),
    ("search.default_keep_alive", "5m"),
    ("search.default_search_timeout", "-1"),
    ("search.low_level_cancellation", "true"),
    ("search.max_aggregation_rewrite_filters", "3000"),
    ("search.max_buckets", "65535"),
    ("search.max_keep_alive", "24h"),
    ("search.max_open_scroll_context", "500"),
    ("transport.compress", "false"),
    ("transport.connect_timeout", "30s"),
];

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
        // what this node was configured with, and what the engine adds
        for (k, v) in crate::cluster::identity().attributes.iter() {
            defaults[format!("node.attr.{k}")] = json!(v.as_str().unwrap_or(""));
        }
        for (k, v) in node_attrs() {
            let key = format!("node.attr.{k}");
            if defaults.get(&key).is_none() {
                defaults[key] = json!(v);
            }
        }
        // the remotes this node was started knowing about: the suite reads
        // one out of the defaults and registers it again under another name
        for (key, value) in super::configured_defaults() {
            defaults[key] = value;
        }
        // What a setting nobody set comes to. Only the node attributes were
        // listed, so a caller asking what the disk watermarks or the bucket
        // limit are was told nothing; these are the values the reference
        // lists for the settings an operator asks about, and the ones a
        // node works out for itself. A setting set on the cluster is not a
        // default, and is left out.
        let set: std::collections::HashSet<String> = ["persistent", "transient"]
            .iter()
            .filter_map(|scope| raw.get(*scope))
            .flat_map(|v| {
                let mut flat = serde_json::Map::new();
                crate::api::flatten_settings(v, "", &mut flat);
                flat.into_iter().map(|(k, _)| k)
            })
            .collect();
        let me = crate::cluster::identity();
        let cpus = crate::api::num_cpus();
        let mut common: Vec<(String, Value)> =
            COMMON_DEFAULTS.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
        common.extend([
            ("cluster.name".to_string(), json!(me.cluster_name)),
            ("node.name".to_string(), json!(me.name)),
            ("node.roles".to_string(), json!(me.roles)),
            ("thread_pool.search.size".to_string(), json!((cpus * 3 / 2 + 1).to_string())),
            ("thread_pool.write.size".to_string(), json!(cpus.to_string())),
            ("thread_pool.get.size".to_string(), json!(cpus.to_string())),
            ("script.allowed_types".to_string(), json!([])),
            ("script.allowed_contexts".to_string(), json!([])),
            ("cluster.routing.allocation.awareness.attributes".to_string(), json!([])),
        ]);
        for (key, value) in common {
            if !set.contains(&key) && defaults.get(&key).is_none() {
                defaults[key] = value;
            }
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
            // a setting belongs to a part of the server; one whose family is
            // not a family at all is a name the cluster does not know
            let family = k.split('.').next().unwrap_or("");
            if !SETTING_FAMILIES.contains(&family) {
                return err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!("{scope} setting [{k}], not recognized"),
                );
            }
        }
        body[scope] = Value::Object(flat);
    }
    // A remote cluster is reached one of two ways, and each has settings of
    // its own: `seeds` and `node_connections` belong to sniffing, a
    // `proxy_address` and its socket count to a proxy. Naming the other
    // mode's settings is a mistake the reference refuses, in these words;
    // this took them and connected by whichever it looked at first.
    if let Some(refusal) = remote_mode_complaint(&store, &body) {
        return refusal;
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
        // the same in the nested shape: a removal is not echoed back as a
        // key holding null, which is what `transient: {}` asserts after one
        Some(v) => {
            let mut o = v.as_object().cloned().unwrap_or_default();
            o.retain(|_, val| !val.is_null());
            nest_settings(&Value::Object(o))
        }
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

/// A time value (`30s`, `500ms`, `2m`, a bare number of milliseconds) in milliseconds.
fn parse_time_ms(t: &str) -> Option<u64> {
    let t = t.trim();
    let (num, unit) = match t.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => (&t[..i], &t[i..]),
        None => (t, "ms"),
    };
    let n: f64 = num.trim().parse().ok()?;
    let mult = match unit {
        "ms" => 1.0,
        "s" => 1_000.0,
        "m" => 60_000.0,
        "h" => 3_600_000.0,
        "d" => 86_400_000.0,
        _ => return None,
    };
    Some((n * mult) as u64)
}

/// `POST /_boost/chaos` -- a partition made real at this node, for the
/// chaos and linearizability runs; mounted only under `BOOSTSEARCH_CHAOS=1`.
/// `{"cut": ["n2", "n3"]}` cuts this node off from those (by name or id);
/// `{"heal": true}` mends every cut.
pub async fn chaos(State(_store): State<Store>, body: String) -> Response {
    let v: Value = parse_body(&body).unwrap_or(json!({}));
    let Some(t) = crate::cluster::tcp::global() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "exception", "no transport");
    };
    if v.get("heal").and_then(|h| h.as_bool()).unwrap_or(false) {
        t.heal();
        return (StatusCode::OK, axum::Json(json!({"healed": true}))).into_response();
    }
    let names: Vec<String> = v
        .get("cut")
        .and_then(|c| c.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let live = crate::cluster::current_state();
    let ids: Vec<crate::cluster::NodeId> = names
        .iter()
        .filter_map(|n| {
            live.nodes.values().find(|d| d.name == *n || d.id.as_str() == n).map(|d| d.id.clone())
        })
        .collect();
    if ids.len() != names.len() {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "a node named is not in the cluster",
        );
    }
    t.cut(&ids);
    (StatusCode::OK, axum::Json(json!({"cut": ids.iter().map(|i| i.as_str()).collect::<Vec<_>>()})))
        .into_response()
}

/// A remote cluster's settings that do not belong to the mode it is in.
fn remote_mode_complaint(store: &Store, body: &Value) -> Option<Response> {
    const SNIFF_ONLY: &[&str] = &["seeds", "node_connections"];
    const PROXY_ONLY: &[&str] = &["proxy_address", "proxy_socket_connections", "server_name"];
    let stored = store.cluster_settings();
    for scope in ["persistent", "transient"] {
        let Some(asked) = body.get(scope).and_then(|v| v.as_object()) else { continue };
        // the keys flat, however they were written
        let mut asked_flat = serde_json::Map::new();
        crate::api::settings::flatten_settings(&Value::Object(asked.clone()), "", &mut asked_flat);
        // What the remote will have once this is applied: what is stored for
        // it already, with this request laid over it -- a null in the request
        // takes a stored key away. Switching an existing sniff cluster to
        // proxy while its seeds are still set is the same mistake as naming
        // both at once, and was only caught when both were in one request.
        let mut flat = serde_json::Map::new();
        for sc in ["persistent", "transient"] {
            if let Some(o) = stored.get(sc).and_then(|v| v.as_object()) {
                let mut f = serde_json::Map::new();
                crate::api::settings::flatten_settings(&Value::Object(o.clone()), "", &mut f);
                for (k, v) in f {
                    if k.starts_with("cluster.remote.") {
                        flat.insert(k, v);
                    }
                }
            }
        }
        for (k, v) in &asked_flat {
            if v.is_null() {
                flat.remove(k);
            } else {
                flat.insert(k.clone(), v.clone());
            }
        }
        let mut names: Vec<String> = asked_flat
            .keys()
            .filter_map(|k| k.strip_prefix("cluster.remote."))
            .filter_map(|rest| rest.rsplit_once('.').map(|(n, _)| n.to_string()))
            .collect();
        names.sort();
        names.dedup();
        for name in names {
            let key = |leaf: &str| format!("cluster.remote.{name}.{leaf}");
            // the mode it will be in: what this request says, else what it is
            let mode = flat
                .get(&key("mode"))
                .and_then(|v| v.as_str())
                .map(|m| m.to_ascii_lowercase())
                .or_else(|| {
                    ["transient", "persistent"].iter().find_map(|sc| {
                        stored
                            .get(*sc)
                            .and_then(|v| v.get(key("mode")))
                            .and_then(|v| v.as_str())
                            .map(|m| m.to_ascii_lowercase())
                    })
                })
                .unwrap_or_else(|| "sniff".to_string());
            let (wrong, required) =
                if mode == "proxy" { (SNIFF_ONLY, "SNIFF") } else { (PROXY_ONLY, "PROXY") };
            for leaf in wrong {
                let k = key(leaf);
                if flat.get(&k).map(|v| !v.is_null()).unwrap_or(false) {
                    return Some(err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        format!(
                            "Setting \"{k}\" cannot be used with the configured \"{}\" \
                             [required={required}, configured={}]",
                            key("mode"),
                            mode.to_ascii_uppercase()
                        ),
                    ));
                }
            }
        }
    }
    None
}
