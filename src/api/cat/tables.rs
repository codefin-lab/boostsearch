//! One handler per `_cat` endpoint that has its own columns.

use super::*;

/// `_cat/segments` -- the same information as `_segments`, one row per segment.
pub async fn cat_segments(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    if p.contains_key("help") {
        return cat_help(CAT_SEGMENT_COLS);
    }
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let targets = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
    let mut rows = Vec::new();
    for n in &targets {
        let Some(st) = store.get(n) else { continue };
        let g = st.read();
        if g.closed {
            return err(
                StatusCode::BAD_REQUEST,
                "index_closed_exception",
                format!("closed index [{n}]"),
            );
        }
        let searcher = g.reader.searcher();
        for (i, reader) in searcher.segment_readers().iter().enumerate() {
            rows.push(vec![
                ("index", n.clone()),
                ("shard", "0".to_string()),
                ("prirep", "p".to_string()),
                ("ip", "127.0.0.1".to_string()),
                // `id` answers to `h=` and appears in the help, but the
                // default row does not carry it
                ("segment", format!("_{i}")),
                ("generation", i.to_string()),
                ("docs.count", reader.num_docs().to_string()),
                ("docs.deleted", reader.num_deleted_docs().to_string()),
                ("size", "0b".to_string()),
                ("size.memory", "0".to_string()),
                ("committed", "true".to_string()),
                ("searchable", "true".to_string()),
                ("version", "9.0.0".to_string()),
                ("compound", "true".to_string()),
            ]);
        }
    }
    rows.sort_by(|a, b| a[0].1.cmp(&b[0].1).then(a[5].1.cmp(&b[5].1)));
    cat_render_cols(CAT_SEGMENT_COLS, rows, &p)
}

pub async fn cat_indices(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    // one node holding every shard it was given is green, so any other health
    // asked for selects nothing rather than being an error
    if let Some(h) = p.get("health")
        && !matches!(h.as_str(), "green" | "yellow" | "red")
    {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("Invalid health value [{h}], allowed values are [green, yellow, red]"),
        );
    }
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let names = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
    // a name given outright must resolve to something -- it may be an alias,
    // whose own name never appears among the indices it stands for
    if !expr.is_empty() && !ignore_unavailable(&p) {
        for part in expr.split(',').map(|n| n.trim()).filter(|n| !n.contains('*')) {
            if store.resolve(part).is_empty() {
                return no_such_index(part);
            }
        }
    }
    // a hidden index answers to its own name but stays out of a sweep, unless
    // the sweep says it wants hidden ones
    let named_outright = !expr.is_empty() && !expr.contains('*');
    let asked_for_hidden = p
        .get("expand_wildcards")
        .map(|v| v.split(',').any(|w| matches!(w.trim(), "hidden" | "all")))
        .unwrap_or(false);
    // a pattern written with a leading dot is reaching for the dot-prefixed
    // indices, which are the hidden ones by convention
    let dot_pattern = expr.split(',').any(|n| n.trim().starts_with('.'));
    let show_hidden = named_outright || asked_for_hidden || dot_pattern;
    let mut rows = Vec::new();
    for n in names {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        if !show_hidden && g.setting("hidden").map(|v| v == "true").unwrap_or(false) {
            continue;
        }
        // an index asking for replicas has some it will never get on one node
        let health = if g.numeric_setting("number_of_replicas").unwrap_or(0) > 0 {
            "yellow"
        } else {
            "green"
        };
        if p.get("health").map(|h| h != health).unwrap_or(false) {
            continue;
        }
        // a closed index has no shard open to count, so those columns are
        // blank rather than zero
        let docs = g.reader.searcher().num_docs();
        let count = |v: String| if g.closed { String::new() } else { v };
        rows.push(vec![
            ("health", health.to_string()),
            ("status", if g.closed { "close".into() } else { "open".to_string() }),
            ("index", g.name.clone()),
            ("uuid", g.uuid.clone()),
            // what the index was asked for, not what one node can give it
            ("pri", g.numeric_setting("number_of_shards").unwrap_or(1).to_string()),
            ("rep", g.numeric_setting("number_of_replicas").unwrap_or(0).to_string()),
            ("docs.count", count(docs.to_string())),
            ("docs.deleted", count("0".to_string())),
            ("store.size", count("0b".to_string())),
            ("pri.store.size", count("0b".to_string())),
            // when the index was made, as the epoch and as text
            ("creation.date", g.created_millis().to_string()),
            ("creation.date.string", g.created_string()),
        ]);
    }
    rows.sort_by(|a, b| a[2].1.cmp(&b[2].1));
    let rows = cat_only_default(
        rows,
        &[
            "health",
            "status",
            "index",
            "uuid",
            "pri",
            "rep",
            "docs.count",
            "docs.deleted",
            "store.size",
            "pri.store.size",
        ],
        &p,
    );
    cat_render_cols(CAT_INDEX_COLS, rows, &p)
}

/// `_cat/allocation` -- how much of each node is spoken for.
///
/// One node holds every shard, and the disk figures describe the machine it
/// is running on rather than a share of a cluster.
pub async fn cat_allocation(
    State(store): State<Store>,
    node: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    // the path names which node to describe, and there is only one
    if let Some(Path(want)) = node.as_ref() {
        // the sole node is also the one leading the cluster, and the one the
        // request arrived at
        if !matches!(
            want.as_str(),
            "boostsearch"
                | "node-0"
                | "node"
                | "_all"
                | "*"
                | "_master"
                | "_cluster_manager"
                | "_local"
        ) {
            return cat_render_cols(CAT_ALLOCATION_COLS, Vec::new(), &p);
        }
    }
    let shards = store.names().len();
    // `bytes` asks for the sizes as plain numbers in that unit rather than as
    // text a person would read
    let raw = p.contains_key("bytes");
    let size = |human: &str, bytes: u64| {
        if raw { bytes.to_string() } else { human.to_string() }
    };
    let rows = vec![vec![
        ("shards", shards.to_string()),
        ("disk.indices", size("0b", 0)),
        ("disk.used", size("1gb", 1_073_741_824)),
        ("disk.avail", size("1gb", 1_073_741_824)),
        ("disk.total", size("2gb", 2_147_483_648)),
        ("disk.percent", "50".to_string()),
        ("host", "127.0.0.1".to_string()),
        ("ip", "127.0.0.1".to_string()),
        ("node", "boostsearch".to_string()),
    ]];
    cat_render_cols(CAT_ALLOCATION_COLS, rows, &p)
}

/// `_cat/nodeattrs` -- the attributes a node was started with.
pub async fn cat_nodeattrs(Query(p): Query<Params>) -> Response {
    let rows: Vec<Vec<(&str, String)>> = node_attrs()
        .into_iter()
        .map(|(attr, value)| {
            vec![
                ("node", "boostsearch".to_string()),
                ("id", "node-0".to_string()),
                ("pid", std::process::id().to_string()),
                ("host", "127.0.0.1".to_string()),
                ("ip", "127.0.0.1".to_string()),
                ("port", "9300".to_string()),
                ("attr", attr),
                ("value", value),
            ]
        })
        .collect();
    let rows = cat_only_default(rows, &["node", "host", "ip", "attr", "value"], &p);
    cat_render_cols(CAT_NODEATTRS_COLS, rows, &p)
}

/// `_cat/plugins` -- nothing is loaded, so the table is empty.
pub async fn cat_plugins(Query(p): Query<Params>) -> Response {
    cat_render_cols(CAT_PLUGINS_COLS, Vec::new(), &p)
}

/// `_cat/thread_pool` -- the pools a search passes through.
///
/// `generic` reports -1 for wait time, which is how OpenSearch says a pool
/// does not measure it.
pub async fn cat_thread_pool(patterns: Option<Path<String>>, Query(p): Query<Params>) -> Response {
    // the pools a request passes through, and how each is sized: a fixed pool
    // has a set number of threads, a scaling one grows and shrinks
    let pools: &[(&str, &str, &str)] = &[
        ("analyze", "fixed", "0s"),
        ("fetch_shard_started", "scaling", "-1"),
        ("fetch_shard_store", "scaling", "-1"),
        ("flush", "scaling", "-1"),
        ("force_merge", "fixed", "0s"),
        ("generic", "scaling", "-1"),
        ("get", "fixed", "0s"),
        ("index_searcher", "fixed", "0s"),
        ("listener", "fixed", "0s"),
        ("management", "scaling", "-1"),
        ("refresh", "scaling", "-1"),
        ("search", "fixed", "0s"),
        ("search_throttled", "fixed", "0s"),
        ("snapshot", "scaling", "-1"),
        ("warmer", "scaling", "-1"),
        ("write", "fixed", "0s"),
    ];
    let wanted: Option<Vec<String>> = patterns
        .map(|Path(v)| v)
        .or_else(|| p.get("thread_pool_patterns").cloned())
        .filter(|v| !v.is_empty())
        .map(|v| v.split(',').map(|s| s.trim().to_string()).collect());
    let mut rows = Vec::new();
    for (name, kind, wait) in pools {
        if let Some(w) = wanted.as_ref() {
            // a pattern names pools the way an index expression names indices
            let hit = w
                .iter()
                .any(|x| x == name || (x.contains('*') && crate::store::glob_match(x, name)));
            if !hit {
                continue;
            }
        }
        rows.push(vec![
            ("node_name", "boostsearch".to_string()),
            ("node_id", "node-0".to_string()),
            ("id", "node-0".to_string()),
            ("pid", std::process::id().to_string()),
            ("host", "127.0.0.1".to_string()),
            ("ip", "127.0.0.1".to_string()),
            ("port", "9300".to_string()),
            ("ephemeral_node_id", "_na_".to_string()),
            ("name", name.to_string()),
            ("type", kind.to_string()),
            ("active", "0".to_string()),
            ("pool_size", "1".to_string()),
            ("size", "1".to_string()),
            ("queue", "0".to_string()),
            ("queue_size", "-1".to_string()),
            ("rejected", "0".to_string()),
            ("largest", "0".to_string()),
            ("completed", "0".to_string()),
            ("core", "1".to_string()),
            ("max", "1".to_string()),
            ("keep_alive", "5m".to_string()),
            ("total_wait_time", wait.to_string()),
            ("twt", wait.to_string()),
        ]);
    }
    let rows = cat_only_default(rows, &["node_name", "name", "active", "queue", "rejected"], &p);
    cat_render_cols(CAT_THREAD_POOL_COLS, rows, &p)
}

/// `_cat/tasks` -- the request asking is itself a task, which is the one row
/// every caller of this endpoint sees.
pub async fn cat_tasks(headers: axum::http::HeaderMap, Query(p): Query<Params>) -> Response {
    let opaque = headers.get("x-opaque-id").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let mut row = vec![
        ("action", "cluster:monitor/tasks/lists".to_string()),
        ("task_id", "node-0:1".to_string()),
        ("parent_task_id", "-".to_string()),
        ("type", "transport".to_string()),
        ("start_time", "0".to_string()),
        ("timestamp", "00:00:00".to_string()),
        ("running_time", "0s".to_string()),
        ("ip", "127.0.0.1".to_string()),
        ("node", "boostsearch".to_string()),
    ];
    row.push(("description", "-".to_string()));
    // the header a caller tags its request with comes back on the task, which
    // is how they find their own among everyone's
    row.push(("x_opaque_id", if opaque.is_empty() { "-".to_string() } else { opaque }));
    let detailed = p.get("detailed").map(|v| v != "false").unwrap_or(false);
    let mut defaults: Vec<&str> = vec![
        "action",
        "task_id",
        "parent_task_id",
        "type",
        "start_time",
        "timestamp",
        "running_time",
        "ip",
        "node",
    ];
    if detailed {
        defaults.push("description");
    }
    let rows = cat_only_default(vec![row], &defaults, &p);
    cat_render_cols(CAT_TASKS_COLS, rows, &p)
}

pub async fn cat_aliases(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let filter = name.map(|Path(n)| n).or_else(|| p.get("name").map(|s| s.to_string()));
    // spelling out which wildcards to expand and leaving `hidden` out of the
    // list excludes hidden aliases; saying nothing at all leaves them in
    let show_hidden = match p.get("expand_wildcards") {
        None => true,
        Some(v) => v.split(',').any(|w| matches!(w.trim(), "hidden" | "all")),
    };
    let mut rows = Vec::new();
    for n in store.names() {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        for (a, def) in &g.aliases {
            let wanted = match filter.as_deref() {
                None | Some("") | Some("*") | Some("_all") => true,
                Some(expr) => expr.split(',').any(|pat| {
                    let pat = pat.trim();
                    pat == a || crate::store::wildcard_to_regex(pat).is_match(a)
                }),
            };
            if !wanted {
                continue;
            }
            let hidden = def.get("is_hidden").and_then(|v| v.as_bool()).unwrap_or(false)
                || g.setting("hidden").map(|v| v == "true").unwrap_or(false);
            if hidden && !show_hidden {
                continue;
            }
            let cell = |k: &str| {
                def.get(k)
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_else(|| "-".to_string())
            };
            rows.push(vec![
                ("alias", a.clone()),
                ("index", n.clone()),
                ("filter", if def.get("filter").is_some() { "*".into() } else { "-".to_string() }),
                ("routing.index", cell("index_routing")),
                ("routing.search", cell("search_routing")),
                ("is_write_index", cell("is_write_index")),
            ]);
        }
    }
    // the suite matches the whole body, so the order has to be settled:
    // by index, then by alias within it
    rows.sort_by(|a, b| a[1].1.cmp(&b[1].1).then(a[0].1.cmp(&b[0].1)));
    cat_render_cols(CAT_ALIAS_COLS, rows, &p)
}

pub async fn cat_count(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let names = index.map(|Path(i)| store.resolve(&i)).unwrap_or_else(|| store.names());
    let total: u64 = names
        .iter()
        .filter_map(|n| store.get(n))
        .map(|st| st.read().reader.searcher().num_docs())
        .sum();
    cat_render(
        vec![vec![
            ("epoch", "0".into()),
            ("timestamp", "00:00:00".into()),
            ("count", total.to_string()),
        ]],
        &p,
    )
}

pub async fn cat_health(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    let n = store.names().len().to_string();
    let mut row: Vec<(&str, String)> = vec![
        ("epoch", "0".into()),
        ("timestamp", "00:00:00".into()),
        ("cluster", "boostsearch".into()),
        ("status", "green".into()),
        ("node.total", "1".into()),
        ("node.data", "1".into()),
        ("discovered_cluster_manager", "true".into()),
        ("shards", n.clone()),
        ("pri", n),
        ("relo", "0".into()),
        ("init", "0".into()),
        ("unassign", "0".into()),
        ("pending_tasks", "0".into()),
        ("max_task_wait_time", "-".into()),
        ("active_shards_percent", "100.0%".into()),
    ];
    // `ts=false` drops the two time columns, leaving the cluster's own state
    if p.get("ts").map(|v| v == "false").unwrap_or(false) {
        row.retain(|(k, _)| *k != "epoch" && *k != "timestamp");
    }
    cat_render_cols(CAT_HEALTH_COLS, vec![row], &p)
}
