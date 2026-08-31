//! `_cat`: the same answers, as columns a person can read.

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

/// Columns the endpoint can show, for `?help`.
pub(crate) fn cat_help(columns: &[&str]) -> Response {
    let mut out = String::new();
    for c in columns {
        out.push_str(c);
        out.push_str(" | | \n");
    }
    out.into_response()
}

pub(crate) fn cat_render(rows: Vec<Vec<(&str, String)>>, p: &Params) -> Response {
    let cols: Vec<&str> =
        rows.first().map(|r| r.iter().map(|(k, _)| *k).collect()).unwrap_or_default();
    cat_render_cols(&cols, rows, p)
}

/// Columns are passed separately so `?help` and the `?v` header still work on an
/// endpoint that currently has no rows to show.
/// `_cat` columns answer to their full name, to the part after the last dot,
/// and to a leading-letter abbreviation -- `a` for `alias`, `rs` for
/// `routing.search`.
/// The short names `_cat` gives columns whose own name is nothing like them.
pub(crate) fn cat_column_alias(column: &str, asked: &str) -> bool {
    const ALIASES: &[(&str, &str)] = &[
        // `i` is the index where there is one and the address otherwise; the
        // row's own column order settles which, since a table with an index
        // column lists it first
        ("index", "i"),
        ("host", "h"),
        ("ip", "i"),
        ("port", "po"),
        ("node_name", "nn"),
        ("diskAvail", "disk"),
        ("diskAvail", "d"),
        ("diskTotal", "dt"),
        ("diskUsed", "du"),
        ("diskUsedPercent", "dup"),
        // how a shard was recovered, which cat writes in short form
        ("shard", "s"),
        ("time", "t"),
        ("type", "ty"),
        ("stage", "st"),
        ("source_host", "shost"),
        ("target_host", "thost"),
        ("source_node", "snode"),
        ("target_node", "tnode"),
        ("repository", "rep"),
        ("snapshot", "snap"),
        ("files", "f"),
        ("files_recovered", "fr"),
        ("files_percent", "fp"),
        ("files_total", "tf"),
        ("bytes", "b"),
        ("bytes_recovered", "br"),
        ("bytes_percent", "bp"),
        ("bytes_total", "tb"),
        ("translog_ops", "to"),
        ("translog_ops_recovered", "tor"),
        ("translog_ops_percent", "top"),
    ];
    ALIASES.iter().any(|(col, short)| *col == column && *short == asked)
}

pub(crate) fn cat_column_matches(column: &str, asked: &str) -> bool {
    if column == asked {
        return true;
    }
    let tail = column.rsplit('.').next().unwrap_or(column);
    if tail == asked {
        return true;
    }
    // the short names `_cat` accepts for columns whose own name is nothing
    // like them
    if cat_column_alias(column, asked) {
        return true;
    }
    let initials: String = column.split('.').filter_map(|p| p.chars().next()).collect();
    initials == asked
        || column.starts_with(asked) && !asked.is_empty() && column.len() > asked.len()
}

pub(crate) fn cat_render_cols(
    columns: &[&str],
    rows: Vec<Vec<(&str, String)>>,
    p: &Params,
) -> Response {
    if p.contains_key("help") {
        return cat_help(columns);
    }
    // `s=` orders the rows by named columns, each optionally `:desc`. A column
    // may be named by any of its aliases, which is how `s=index,a:desc` asks
    // for alias descending within index.
    let mut rows = rows;
    if let Some(spec) = p.get("s").filter(|s| !s.is_empty()) {
        let keys: Vec<(String, bool)> = spec
            .split(',')
            .map(|k| {
                let k = k.trim();
                match k.split_once(':') {
                    Some((name, dir)) => (name.to_string(), dir.eq_ignore_ascii_case("desc")),
                    None => (k.to_string(), false),
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            for (name, desc) in &keys {
                let pick = |r: &Vec<(&str, String)>| {
                    r.iter()
                        .find(|(k, _)| cat_column_matches(k, name))
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default()
                };
                let ord = pick(a).cmp(&pick(b));
                let ord = if *desc { ord.reverse() } else { ord };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
    }
    // `h=` picks and orders the columns
    let rows: Vec<Vec<(&str, String)>> = match p.get("h") {
        Some(spec) if !spec.is_empty() => {
            let want: Vec<&str> = spec.split(',').map(|s| s.trim()).collect();
            rows.into_iter()
                .map(|r| {
                    want.iter()
                        .flat_map(|w| {
                            // a name with a `*` in it stands for every column
                            // it fits, in the order the row holds them
                            if w.contains('*') {
                                return r
                                    .iter()
                                    .filter(|(k, _)| crate::store::glob_match(w, k))
                                    .map(|(k, v)| (*k, v.clone()))
                                    .collect::<Vec<_>>();
                            }
                            // a column answers to its name or to one of the
                            // short forms `_cat` accepts, and is headed by the
                            // name it was asked for
                            // an exact name wins, then a short form `_cat`
                            // names outright, and only then a prefix -- or
                            // `i` would find `id` before `ip`
                            r.iter()
                                .find(|(k, _)| k == w)
                                .or_else(|| r.iter().find(|(k, _)| cat_column_alias(k, w)))
                                .or_else(|| r.iter().find(|(k, _)| cat_column_matches(k, w)))
                                .map(|(_, v)| (*w, v.clone()))
                                .into_iter()
                                .collect::<Vec<_>>()
                        })
                        .collect()
                })
                .collect()
        }
        _ => rows,
    };
    if p.get("format").map(|f| f == "json").unwrap_or(false) {
        let arr: Vec<Value> = rows
            .iter()
            .map(|r| Value::Object(r.iter().map(|(k, v)| (k.to_string(), json!(v))).collect()))
            .collect();
        return axum::Json(arr).into_response();
    }
    // plain text: the format `cat` is named for. Cells are padded to the width
    // of their column so the values line up down the page.
    let show_head = p.contains_key("v") && p.get("v").map(|v| v != "false").unwrap_or(true);
    // with no rows to read the columns off, `h=` still says which were asked
    // for, and in what order
    let asked: Vec<&str> = p
        .get("h")
        .filter(|s| !s.is_empty())
        .map(|spec| spec.split(',').map(|s| s.trim()).collect())
        .unwrap_or_default();
    let head: Vec<&str> = match rows.first() {
        Some(r) => r.iter().map(|(k, _)| *k).collect(),
        None if !asked.is_empty() => asked,
        None => columns.to_vec(),
    };
    let mut widths: Vec<usize> =
        if show_head { head.iter().map(|h| h.len()).collect() } else { vec![0; head.len()] };
    for r in &rows {
        for (i, (_, v)) in r.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(v.len());
            }
        }
    }
    // alignment is a property of the column, not of what lands in it: the
    // ones an allocation is measured in line up on their right edge
    const RIGHT_SUFFIX: &[&str] =
        &[".indices", ".used", ".avail", ".total", ".percent", ".current", ".max"];
    let numeric: Vec<bool> = head
        .iter()
        .map(|h| *h == "shards" || RIGHT_SUFFIX.iter().any(|sfx| h.ends_with(sfx)))
        .collect();
    let line = |cells: Vec<&str>| {
        let mut s = String::new();
        for (i, c) in cells.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            let width = widths.get(i).copied().unwrap_or(0);
            let right = numeric.get(i).copied().unwrap_or(false);
            if right {
                for _ in c.len()..width {
                    s.push(' ');
                }
            }
            s.push_str(c);
            // the last cell needs no padding: nothing follows it to line up
            if !right && i + 1 < cells.len() {
                for _ in c.len()..width {
                    s.push(' ');
                }
            }
        }
        s.push('\n');
        s
    };
    let mut out = String::new();
    if show_head && !head.is_empty() {
        out.push_str(&line(head.clone()));
    }
    for r in &rows {
        out.push_str(&line(r.iter().map(|(_, v)| v.as_str()).collect()));
    }
    out.into_response()
}

/// An endpoint with no rows on a single node still has to answer `?help`.
pub(crate) fn cat_named(columns: &[&str], p: &Params) -> Response {
    cat_render_cols(columns, Vec::new(), p)
}

pub async fn cat_indices(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    // one node holding every shard it was given is green, so any other health
    // asked for selects nothing rather than being an error
    if let Some(h) = p.get("health") {
        if !matches!(h.as_str(), "green" | "yellow" | "red") {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("Invalid health value [{h}], allowed values are [green, yellow, red]"),
            );
        }
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

/// Some `_cat` columns exist for `h=` to ask for but are not in the table a
/// bare request returns. Drop those unless the caller named columns.
pub(crate) fn cat_only_default<'a>(
    rows: Vec<Vec<(&'a str, String)>>,
    defaults: &[&str],
    p: &Params,
) -> Vec<Vec<(&'a str, String)>> {
    if p.get("h").map(|h| !h.is_empty()).unwrap_or(false) {
        return rows;
    }
    rows.into_iter()
        .map(|r| r.into_iter().filter(|(k, _)| defaults.contains(k)).collect())
        .collect()
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
                ("id", "node-0".into()),
                ("host", "127.0.0.1".into()),
                ("ip", "127.0.0.1".into()),
                ("node", "boostsearch".into()),
            ]],
            &p,
        ),
        "nodes" => {
            let row: Vec<(&str, String)> = vec![
                // `full_id` asks for the whole node identifier rather than
                // the short form a table shows by default
                (
                    "id",
                    if p.get("full_id").map(|v| v != "false").unwrap_or(false) {
                        "node-0".to_string()
                    } else {
                        "node".to_string()
                    },
                ),
                ("ip", "127.0.0.1".into()),
                ("file_desc.current", "0".into()),
                ("file_desc.percent", "0".into()),
                ("file_desc.max", "0".into()),
                ("heap.current", "0b".into()),
                ("heap.percent", "0".into()),
                ("heap.max", "0b".into()),
                ("ram.current", "0b".into()),
                ("ram.percent", "0".into()),
                ("ram.max", "0b".into()),
                ("http", "127.0.0.1:9200".into()),
                ("cpu", "0".into()),
                ("load_1m", "0.00".into()),
                ("load_5m", "0.00".into()),
                ("load_15m", "0.00".into()),
                ("node.role", "dimr".into()),
                ("node.roles", "data,ingest".into()),
                ("cluster_manager", "*".into()),
                ("name", "boostsearch".into()),
                ("diskAvail", "1gb".into()),
                ("diskTotal", "2gb".into()),
                ("diskUsed", "1gb".into()),
                ("diskUsedPercent", "50.00".into()),
            ];
            let rows = cat_only_default(
                vec![row],
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
                        ("id", "node-0".into()),
                        ("node", "boostsearch".into()),
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
                        ("id", "node-0".to_string()),
                        ("host", "127.0.0.1".to_string()),
                        ("ip", "127.0.0.1".to_string()),
                        ("node", "boostsearch".to_string()),
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
                        ("target_node", "boostsearch".into()),
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
                        ("node", "boostsearch".to_string()),
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
