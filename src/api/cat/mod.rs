//! `_cat`: the same answers, as columns a person can read.

use super::*;

mod render;
pub(crate) use render::*;
mod tables;
pub use tables::*;

/// `_cat/{what}` in one place. The shapes people actually read are filled in;
/// the rest answer with the right envelope rather than a 501.
pub async fn cat_dispatch_target(
    State(store): State<Store>,
    Path((what, target)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    cat_by_name(store, what, Some(target), p).await
}

pub async fn cat_dispatch(
    State(store): State<Store>,
    Path(what): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    cat_by_name(store, what, None, p).await
}

pub(crate) async fn cat_by_name(
    store: Store,
    what: String,
    target: Option<String>,
    p: Params,
) -> Response {
    let what = what.split('/').next().unwrap_or("").to_string();
    match what.as_str() {
        "indices" => cat_indices(State(store), None, Query(p)).await,
        "aliases" => cat_aliases(State(store), None, Query(p)).await,
        "count" => cat_count(State(store), None, Query(p)).await,
        "health" => cat_health(State(store), Query(p)).await,
        "master" | "cluster_manager" => cat_render(
            vec![vec![
                ("id", crate::cluster::identity().id.as_str().into()),
                ("host", "127.0.0.1".into()),
                ("ip", "127.0.0.1".into()),
                ("node", crate::cluster::identity().name.clone().into()),
            ]],
            &p,
        ),
        "nodes" => {
            // one row per node the cluster state holds, the manager starred
            let live = crate::cluster::current_state();
            let full = p.get("full_id").map(|v| v != "false").unwrap_or(false);
            let mut rows_all: Vec<Vec<(&str, String)>> = Vec::new();
            for (id, n) in &live.nodes {
                let letters: String = {
                    let mut l = String::new();
                    for r in &n.roles {
                        l.push(match r.as_str() {
                            "cluster_manager" | "master" => 'm',
                            "data" => 'd',
                            "ingest" => 'i',
                            "remote_cluster_client" => 'r',
                            "search" => 's',
                            "warm" => 'w',
                            _ => continue,
                        });
                    }
                    let mut v: Vec<char> = l.chars().collect();
                    v.sort();
                    v.into_iter().collect()
                };
                let ip = n
                    .transport_address
                    .rsplit_once(':')
                    .map(|(h, _)| h.to_string())
                    .unwrap_or_default();
                let is_manager = live.cluster_manager.as_ref() == Some(id);
                rows_all.push(vec![
                    (
                        "id",
                        if full {
                            id.as_str().to_string()
                        } else {
                            id.as_str().chars().take(4).collect()
                        },
                    ),
                    ("ip", ip.clone()),
                    ("file_desc.current", "0".into()),
                    ("file_desc.percent", "0".into()),
                    ("file_desc.max", "0".into()),
                    ("heap.current", "0b".into()),
                    ("heap.percent", "0".into()),
                    ("heap.max", "0b".into()),
                    ("ram.current", "0b".into()),
                    ("ram.percent", "0".into()),
                    ("ram.max", "0b".into()),
                    ("http", format!("{ip}:9200")),
                    ("cpu", "0".into()),
                    ("load_1m", "0.00".into()),
                    ("load_5m", "0.00".into()),
                    ("load_15m", "0.00".into()),
                    ("node.role", letters),
                    ("node.roles", n.roles.join(",")),
                    ("cluster_manager", if is_manager { "*".into() } else { "-".into() }),
                    ("name", n.name.clone()),
                    ("diskAvail", "1gb".into()),
                    ("diskTotal", "2gb".into()),
                    ("diskUsed", "1gb".into()),
                    ("diskUsedPercent", "50.00".into()),
                ]);
            }
            let rows = cat_only_default(
                rows_all,
                &[
                    "ip",
                    "heap.percent",
                    "ram.percent",
                    "cpu",
                    "load_1m",
                    "load_5m",
                    "load_15m",
                    "node.role",
                    "node.roles",
                    "cluster_manager",
                    "name",
                ],
                &p,
            );
            cat_render_cols(
                &[
                    "id",
                    "ip",
                    "file_desc.current",
                    "file_desc.percent",
                    "file_desc.max",
                    "heap.current",
                    "heap.percent",
                    "heap.max",
                    "ram.current",
                    "ram.percent",
                    "ram.max",
                    "http",
                    "cpu",
                    "load_1m",
                    "load_5m",
                    "load_15m",
                    "node.role",
                    "node.roles",
                    "cluster_manager",
                    "name",
                    "diskAvail",
                    "diskTotal",
                    "diskUsed",
                    "diskUsedPercent",
                ],
                rows,
                &p,
            )
        }
        "templates" => {
            let mut rows: Vec<Vec<(&str, String)>> = store
                .get_templates()
                .into_iter()
                .map(|(name, t)| {
                    let list = |key: &str| {
                        t.get(key)
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                // a list is written the way a list is read, with
                                // a space after each comma
                                a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")
                            })
                            .unwrap_or_default()
                    };
                    // the composable form keeps the body it was written with,
                    // which is where its version and components are
                    let body = t.get("__composable").unwrap_or(&t);
                    let num = |key: &str| {
                        body.get(key)
                            .or_else(|| t.get(key))
                            .map(|o| o.to_string())
                            .unwrap_or_default()
                    };
                    vec![
                        ("name", name),
                        ("index_patterns", format!("[{}]", list("index_patterns"))),
                        ("order", {
                            let o = num("order");
                            if o.is_empty() { num("priority") } else { o }
                        }),
                        ("version", num("version")),
                        ("composed_of", {
                            let c = body
                                .get("composed_of")
                                .and_then(|v| v.as_array())
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|x| x.as_str())
                                        .collect::<Vec<_>>()
                                        .join(",")
                                })
                                .unwrap_or_default();
                            // a template composed of nothing names nothing
                            if c.is_empty() { String::new() } else { format!("[{c}]") }
                        }),
                    ]
                })
                .collect();
            // the path names which templates to show, by name or by pattern
            if let Some(t) = target.as_deref().filter(|t| !t.is_empty() && *t != "*") {
                rows.retain(|r| {
                    t.split(',').any(|pat| {
                        let pat = pat.trim();
                        pat == r[0].1 || crate::store::glob_match(pat, &r[0].1)
                    })
                });
            }
            rows.sort_by(|a, b| a[0].1.cmp(&b[0].1));
            let rows = cat_only_default(rows, CAT_TEMPLATE_COLS, &p);
            cat_render_cols(CAT_TEMPLATE_COLS, rows, &p)
        }
        "shards" => {
            // the path names which indices to describe
            let names = match target.as_deref().filter(|t| !t.is_empty()) {
                Some(t) => store.resolve(t),
                None => store.names(),
            };
            // an index is listed shard by shard, and every one of them is
            // started here since the node holds them all
            let mut rows: Vec<Vec<(&str, String)>> = Vec::new();
            for n in names {
                let Some(st) = store.get(&n) else { continue };
                let g = st.read();
                let docs = g.reader.searcher().num_docs();
                let shards = g.numeric_setting("number_of_shards").unwrap_or(1).max(1);
                let replicas = g.numeric_setting("number_of_replicas").unwrap_or(1);
                for shard in 0..shards {
                    rows.push(vec![
                        ("index", n.clone()),
                        ("shard", shard.to_string()),
                        ("prirep", "p".into()),
                        ("state", "STARTED".into()),
                        // the documents all sit in the one shard that exists
                        ("docs", if shard == 0 { docs.to_string() } else { "0".into() }),
                        ("store", "0b".into()),
                        ("ip", "127.0.0.1".into()),
                        ("id", crate::cluster::identity().id.as_str().into()),
                        ("node", crate::cluster::identity().name.clone().into()),
                    ]);
                    // a replica has nowhere else to live on a single node, so
                    // it is listed and unassigned
                    for _ in 0..replicas {
                        rows.push(vec![
                            ("index", n.clone()),
                            ("shard", shard.to_string()),
                            ("prirep", "r".into()),
                            ("state", "UNASSIGNED".into()),
                            ("docs", String::new()),
                            ("store", String::new()),
                            ("ip", String::new()),
                            ("id", String::new()),
                            ("node", String::new()),
                        ]);
                    }
                }
            }
            rows.sort_by(|a, b| a[0].1.cmp(&b[0].1));
            let rows = cat_only_default(
                rows,
                &["index", "shard", "prirep", "state", "docs", "store", "ip", "node"],
                &p,
            );
            cat_render(rows, &p)
        }
        "segments" => {
            let rows = store
                .names()
                .into_iter()
                .filter_map(|n| store.get(&n).map(|st| (n, st)))
                .flat_map(|(n, st)| {
                    let searcher = st.read().reader.searcher();
                    searcher
                        .segment_readers()
                        .iter()
                        .enumerate()
                        .map(|(i, sr)| {
                            vec![
                                ("index", n.clone()),
                                ("shard", "0".into()),
                                ("prirep", "p".into()),
                                ("segment", format!("_{i}")),
                                ("docs.count", sr.num_docs().to_string()),
                                ("docs.deleted", sr.num_deleted_docs().to_string()),
                            ]
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
            cat_render(rows, &p)
        }
        // shapes with nothing meaningful behind them on a single node; `?help`
        // still has to list the right columns
        // what a field's columns take up is the closest thing here to a
        // fielddata cache, and it is reported per field
        "fielddata" => {
            let mut rows: Vec<Vec<(&str, String)>> = Vec::new();
            for name in store.names() {
                let Some(st) = store.get(&name) else { continue };
                let g = st.read();
                let loaded = g.loaded_fielddata.read().clone();
                let mut fields: Vec<(String, u64)> = g
                    .field_column_bytes()
                    .into_iter()
                    .filter(|(f, _)| loaded.contains(f))
                    .collect();
                fields.sort();
                // the path names which fields to report on
                if let Some(want) = target.as_deref() {
                    fields.retain(|(f, _)| want.split(',').any(|w| w.trim() == f));
                }
                for (field, bytes) in fields {
                    rows.push(vec![
                        ("id", crate::cluster::identity().id.as_str().to_string()),
                        ("host", "127.0.0.1".to_string()),
                        ("ip", "127.0.0.1".to_string()),
                        ("node", crate::cluster::identity().name.clone()),
                        ("field", field),
                        ("size", readable_bytes(bytes)),
                    ]);
                }
            }
            cat_render_cols(&["id", "host", "ip", "node", "field", "size"], rows, &p)
        }
        "allocation" => cat_named(
            &[
                "shards",
                "disk.indices",
                "disk.used",
                "disk.avail",
                "disk.total",
                "disk.percent",
                "host",
                "ip",
                "node",
            ],
            &p,
        ),
        "pending_tasks" => cat_named(&["insertOrder", "timeInQueue", "priority", "source"], &p),
        "plugins" => cat_named(&["name", "component", "version"], &p),
        "thread_pool" => cat_thread_pool(target.map(axum::extract::Path), Query(p)).await,
        // how each shard came to be where it is, one row per shard
        "recovery" => {
            const COLS: &[&str] = &[
                "index",
                "shard",
                "start_time",
                "start_time_millis",
                "stop_time",
                "stop_time_millis",
                "time",
                "type",
                "stage",
                "source_host",
                "source_node",
                "target_host",
                "target_node",
                "repository",
                "snapshot",
                "files",
                "files_recovered",
                "files_percent",
                "files_total",
                "bytes",
                "bytes_recovered",
                "bytes_percent",
                "bytes_total",
                "translog_ops",
                "translog_ops_recovered",
                "translog_ops_percent",
            ];
            let names = match target.as_deref().filter(|t| !t.is_empty()) {
                Some(t) => store.resolve(t),
                None => store.names(),
            };
            let mut rows: Vec<Vec<(&str, String)>> = Vec::new();
            for n in names {
                let Some(st) = store.get(&n) else { continue };
                let g = st.read();
                let existing = g.reader.searcher().num_docs() > 0 || g.closed;
                let kind = if g.restored {
                    "snapshot"
                } else if existing {
                    "existing_store"
                } else {
                    "empty_store"
                };
                for shard in 0..g.shard_count() {
                    rows.push(vec![
                        ("index", n.clone()),
                        ("shard", shard.to_string()),
                        ("start_time", "2020-01-01T00:00:00.000Z".into()),
                        ("start_time_millis", "1577836800000".into()),
                        ("stop_time", "2020-01-01T00:00:00.000Z".into()),
                        ("stop_time_millis", "1577836800000".into()),
                        ("time", "0ms".into()),
                        ("type", kind.into()),
                        ("stage", "done".into()),
                        ("source_host", "n/a".into()),
                        ("source_node", "n/a".into()),
                        ("target_host", "127.0.0.1".into()),
                        ("target_node", crate::cluster::identity().name.clone().into()),
                        ("repository", "n/a".into()),
                        ("snapshot", "n/a".into()),
                        ("files", "0".into()),
                        ("files_recovered", "0".into()),
                        ("files_percent", "100.0%".into()),
                        ("files_total", "0".into()),
                        ("bytes", "0b".into()),
                        ("bytes_recovered", "0b".into()),
                        ("bytes_percent", "100.0%".into()),
                        ("bytes_total", "0b".into()),
                        ("translog_ops", "0".into()),
                        ("translog_ops_recovered", "0".into()),
                        ("translog_ops_percent", "100.0%".into()),
                    ]);
                }
            }
            rows.sort_by(|a, b| {
                (a[0].1.clone(), a[1].1.clone()).cmp(&(b[0].1.clone(), b[1].1.clone()))
            });
            cat_render_cols(COLS, rows, &p)
        }
        "repositories" => {
            let mut rows: Vec<Vec<(&str, String)>> = store
                .repositories()
                .into_iter()
                .map(|(name, def)| {
                    vec![
                        ("id", name),
                        ("type", def.get("type").and_then(|t| t.as_str()).unwrap_or("fs").into()),
                    ]
                })
                .collect();
            rows.sort_by(|a, b| a[0].1.cmp(&b[0].1));
            cat_render_cols(&["id", "type"], rows, &p)
        }
        "snapshots" => {
            const COLS: &[&str] = &[
                "id",
                "status",
                "start_epoch",
                "start_time",
                "end_epoch",
                "end_time",
                "duration",
                "indices",
                "successful_shards",
                "failed_shards",
                "total_shards",
                "reason",
            ];
            let repos: Vec<String> = match target.as_deref().filter(|t| !t.is_empty()) {
                Some(t) => t.split(',').map(|s| s.trim().to_string()).collect(),
                None => store.repositories().into_keys().collect(),
            };
            let mut rows: Vec<Vec<(&str, String)>> = Vec::new();
            for repo in repos {
                for (name, snap) in store.snapshots(&repo) {
                    let n = |k: &str| {
                        snap.pointer(&format!("/shards/{k}")).and_then(|v| v.as_u64()).unwrap_or(0)
                    };
                    let indices = snap["indices"].as_array().map(|a| a.len()).unwrap_or(0);
                    rows.push(vec![
                        ("id", name),
                        ("status", "SUCCESS".into()),
                        ("start_epoch", "0".into()),
                        ("start_time", "00:00:00".into()),
                        ("end_epoch", "0".into()),
                        ("end_time", "00:00:00".into()),
                        ("duration", "0s".into()),
                        ("indices", indices.to_string()),
                        ("successful_shards", n("successful").to_string()),
                        ("failed_shards", n("failed").to_string()),
                        ("total_shards", n("total").to_string()),
                        ("reason", String::new()),
                    ]);
                }
            }
            rows.sort_by(|a, b| a[0].1.cmp(&b[0].1));
            cat_render_cols(COLS, rows, &p)
        }
        "tasks" => cat_named(&["action", "task_id", "parent_task_id", "type", "start_time"], &p),
        "nodeattrs" => {
            let rows: Vec<Vec<(&str, String)>> = node_attrs()
                .into_iter()
                .map(|(attr, value)| {
                    vec![
                        ("node", crate::cluster::identity().name.clone()),
                        ("host", "127.0.0.1".to_string()),
                        ("ip", "127.0.0.1".to_string()),
                        ("attr", attr),
                        ("value", value),
                    ]
                })
                .collect();
            cat_render_cols(&["node", "host", "ip", "attr", "value"], rows, &p)
        }
        other => err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("unknown cat endpoint [{other}]"),
        ),
    }
}
