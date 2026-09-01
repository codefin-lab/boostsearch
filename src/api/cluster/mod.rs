//! The cluster as a whole: its health, its state, its settings.

use super::*;

mod settings;
pub use settings::*;
mod state;
pub use state::*;

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
                "analysis": analysis_stats(&store),
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

/// What the indices define for analysis, and what they use of what is built
/// in: each kind counted over every index that defines one, and over how many
/// indices do.
fn analysis_stats(store: &Store) -> Value {
    use std::collections::BTreeMap;
    // name -> (count, indices holding it)
    type Tally = BTreeMap<String, (u64, std::collections::BTreeSet<String>)>;
    let mut tallies: BTreeMap<&str, Tally> = BTreeMap::new();
    let mut note = |kind: &'static str, name: &str, index: &str| {
        let entry = tallies.entry(kind).or_default().entry(name.to_string()).or_default();
        entry.0 += 1;
        entry.1.insert(index.to_string());
    };
    for name in store.resolve("*") {
        let Some(st) = store.get(&name) else { continue };
        let g = st.read();
        let analysis = g
            .settings
            .pointer("/index/analysis")
            .or_else(|| g.settings.pointer("/analysis"))
            .cloned()
            .unwrap_or(Value::Null);
        let defined = |section: &str| -> Vec<(String, Value)> {
            analysis
                .get(section)
                .and_then(|v| v.as_object())
                .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default()
        };
        let named: std::collections::HashSet<String> = ["char_filter", "tokenizer", "filter"]
            .iter()
            .flat_map(|section| defined(section).into_iter().map(|(k, _)| k))
            .collect();
        for (section, kind) in [
            ("char_filter", "char_filter_types"),
            ("tokenizer", "tokenizer_types"),
            ("filter", "filter_types"),
        ] {
            for (_, spec) in defined(section) {
                if let Some(t) = spec.get("type").and_then(|v| v.as_str()) {
                    note(kind, t, &name);
                }
            }
        }
        for (_, spec) in defined("analyzer") {
            // an analyzer described by its parts is a custom one, whatever it
            // was or was not called
            let kind = match spec.get("type").and_then(|v| v.as_str()) {
                Some(t) if t != "custom" => t.to_string(),
                _ => "custom".to_string(),
            };
            note("analyzer_types", &kind, &name);
            // the parts it names that nobody defined are the built-in ones
            for (section, kind) in [
                ("char_filter", "built_in_char_filters"),
                ("tokenizer", "built_in_tokenizers"),
                ("filter", "built_in_filters"),
            ] {
                let listed: Vec<String> = match spec.get(section) {
                    Some(Value::String(one)) => vec![one.clone()],
                    Some(Value::Array(items)) => {
                        items.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()
                    }
                    _ => Vec::new(),
                };
                for part in listed {
                    if !named.contains(&part) {
                        note(kind, &part, &name);
                    }
                }
            }
        }
        // the analyzers the mapping names that the index never defined
        let own: std::collections::HashSet<String> =
            defined("analyzer").into_iter().map(|(k, _)| k).collect();
        let mut used: Vec<String> = Vec::new();
        fn walk(node: &Value, out: &mut Vec<String>) {
            let Some(o) = node.as_object() else { return };
            for key in ["analyzer", "search_analyzer", "search_quote_analyzer"] {
                if let Some(v) = o.get(key).and_then(|v| v.as_str()) {
                    out.push(v.to_string());
                }
            }
            for (key, v) in o {
                if (key == "properties" || key == "fields")
                    && let Some(inner) = v.as_object()
                {
                    inner.values().for_each(|f| walk(f, out));
                }
            }
        }
        walk(&g.mapping.raw, &mut used);
        for analyzer in used {
            if !own.contains(&analyzer) {
                note("built_in_analyzers", &analyzer, &name);
            }
        }
    }
    let listed = |kind: &str| -> Value {
        tallies
            .get(kind)
            .map(|t| {
                t.iter()
                    .map(|(name, (count, indices))| {
                        json!({"name": name, "count": count, "index_count": indices.len()})
                    })
                    .collect::<Vec<_>>()
            })
            .map(Value::Array)
            .unwrap_or_else(|| json!([]))
    };
    json!({
        "char_filter_types": listed("char_filter_types"),
        "tokenizer_types": listed("tokenizer_types"),
        "filter_types": listed("filter_types"),
        "analyzer_types": listed("analyzer_types"),
        "built_in_char_filters": listed("built_in_char_filters"),
        "built_in_tokenizers": listed("built_in_tokenizers"),
        "built_in_filters": listed("built_in_filters"),
        "built_in_analyzers": listed("built_in_analyzers"),
    })
}
