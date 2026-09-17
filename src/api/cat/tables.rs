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
        let unit = p.get("bytes").map(|s| s.to_string());
        for (i, reader) in searcher.segment_readers().iter().enumerate() {
            let size = g.segment_bytes(reader);
            rows.push(vec![
                ("index", n.clone()),
                ("shard", "0".to_string()),
                ("prirep", "p".to_string()),
                ("ip", "127.0.0.1".to_string()),
                // the node the segment is on: not in the default table, but
                // a caller naming its own columns may ask for it
                ("id", crate::cluster::identity().id.as_str().to_string()),
                ("segment", format!("_{i}")),
                ("generation", i.to_string()),
                ("docs.count", reader.num_docs().to_string()),
                ("docs.deleted", reader.num_deleted_docs().to_string()),
                ("size", crate::api::shared::sized(unit.as_deref(), size)),
                ("size.memory", "0".to_string()),
                ("committed", "true".to_string()),
                ("searchable", "true".to_string()),
                ("version", "9.0.0".to_string()),
                ("compound", "true".to_string()),
            ]);
        }
    }
    let at = |r: &Vec<(&str, String)>, k: &str| {
        r.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone()).unwrap_or_default()
    };
    rows.sort_by(|a, b| {
        at(a, "index").cmp(&at(b, "index")).then(at(a, "segment").cmp(&at(b, "segment")))
    });
    let rows = cat_only_default(
        rows,
        &[
            "index",
            "shard",
            "prirep",
            "ip",
            "segment",
            "generation",
            "docs.count",
            "docs.deleted",
            "size",
            "size.memory",
            "committed",
            "searchable",
            "version",
            "compound",
        ],
        &p,
    );
    cat_render_cols(CAT_SEGMENT_COLS, rows, &p)
}

/// How a statistic is written in a `_cat` cell.
#[derive(Clone, Copy)]
enum Cell {
    Count,
    Bytes,
    Millis,
}

/// The `_cat/indices` columns read off an index's statistics, in the
/// reference's order: the column, its primaries twin, where the value is in
/// `_stats`, and how it is written. They were missing -- `h=segments.count`
/// answered with nothing -- and a caller watching an index from a shell
/// reads them here rather than out of `_stats`.
const INDEX_STAT_COLS: &[(&str, &str, &str, Cell)] = &[
    ("completion.size", "pri.completion.size", "/completion/size_in_bytes", Cell::Bytes),
    (
        "fielddata.memory_size",
        "pri.fielddata.memory_size",
        "/fielddata/memory_size_in_bytes",
        Cell::Bytes,
    ),
    ("fielddata.evictions", "pri.fielddata.evictions", "/fielddata/evictions", Cell::Count),
    (
        "query_cache.memory_size",
        "pri.query_cache.memory_size",
        "/query_cache/memory_size_in_bytes",
        Cell::Bytes,
    ),
    ("query_cache.evictions", "pri.query_cache.evictions", "/query_cache/evictions", Cell::Count),
    (
        "request_cache.memory_size",
        "pri.request_cache.memory_size",
        "/request_cache/memory_size_in_bytes",
        Cell::Bytes,
    ),
    (
        "request_cache.evictions",
        "pri.request_cache.evictions",
        "/request_cache/evictions",
        Cell::Count,
    ),
    (
        "request_cache.hit_count",
        "pri.request_cache.hit_count",
        "/request_cache/hit_count",
        Cell::Count,
    ),
    (
        "request_cache.miss_count",
        "pri.request_cache.miss_count",
        "/request_cache/miss_count",
        Cell::Count,
    ),
    ("flush.total", "pri.flush.total", "/flush/total", Cell::Count),
    ("flush.total_time", "pri.flush.total_time", "/flush/total_time_in_millis", Cell::Millis),
    ("get.current", "pri.get.current", "/get/current", Cell::Count),
    ("get.time", "pri.get.time", "/get/time_in_millis", Cell::Millis),
    ("get.total", "pri.get.total", "/get/total", Cell::Count),
    ("get.exists_time", "pri.get.exists_time", "/get/exists_time_in_millis", Cell::Millis),
    ("get.exists_total", "pri.get.exists_total", "/get/exists_total", Cell::Count),
    ("get.missing_time", "pri.get.missing_time", "/get/missing_time_in_millis", Cell::Millis),
    ("get.missing_total", "pri.get.missing_total", "/get/missing_total", Cell::Count),
    (
        "indexing.delete_current",
        "pri.indexing.delete_current",
        "/indexing/delete_current",
        Cell::Count,
    ),
    (
        "indexing.delete_time",
        "pri.indexing.delete_time",
        "/indexing/delete_time_in_millis",
        Cell::Millis,
    ),
    ("indexing.delete_total", "pri.indexing.delete_total", "/indexing/delete_total", Cell::Count),
    (
        "indexing.index_current",
        "pri.indexing.index_current",
        "/indexing/index_current",
        Cell::Count,
    ),
    (
        "indexing.index_time",
        "pri.indexing.index_time",
        "/indexing/index_time_in_millis",
        Cell::Millis,
    ),
    ("indexing.index_total", "pri.indexing.index_total", "/indexing/index_total", Cell::Count),
    ("indexing.index_failed", "pri.indexing.index_failed", "/indexing/index_failed", Cell::Count),
    ("merges.current", "pri.merges.current", "/merges/current", Cell::Count),
    ("merges.current_docs", "pri.merges.current_docs", "/merges/current_docs", Cell::Count),
    (
        "merges.current_size",
        "pri.merges.current_size",
        "/merges/current_size_in_bytes",
        Cell::Bytes,
    ),
    ("merges.total", "pri.merges.total", "/merges/total", Cell::Count),
    ("merges.total_docs", "pri.merges.total_docs", "/merges/total_docs", Cell::Count),
    ("merges.total_size", "pri.merges.total_size", "/merges/total_size_in_bytes", Cell::Bytes),
    ("merges.total_time", "pri.merges.total_time", "/merges/total_time_in_millis", Cell::Millis),
    ("refresh.total", "pri.refresh.total", "/refresh/total", Cell::Count),
    ("refresh.time", "pri.refresh.time", "/refresh/total_time_in_millis", Cell::Millis),
    (
        "refresh.external_total",
        "pri.refresh.external_total",
        "/refresh/external_total",
        Cell::Count,
    ),
    (
        "refresh.external_time",
        "pri.refresh.external_time",
        "/refresh/external_total_time_in_millis",
        Cell::Millis,
    ),
    ("refresh.listeners", "pri.refresh.listeners", "/refresh/listeners", Cell::Count),
    ("search.fetch_current", "pri.search.fetch_current", "/search/fetch_current", Cell::Count),
    ("search.fetch_time", "pri.search.fetch_time", "/search/fetch_time_in_millis", Cell::Millis),
    ("search.fetch_total", "pri.search.fetch_total", "/search/fetch_total", Cell::Count),
    ("search.open_contexts", "pri.search.open_contexts", "/search/open_contexts", Cell::Count),
    ("search.query_current", "pri.search.query_current", "/search/query_current", Cell::Count),
    ("search.query_time", "pri.search.query_time", "/search/query_time_in_millis", Cell::Millis),
    ("search.query_total", "pri.search.query_total", "/search/query_total", Cell::Count),
    ("search.query_failed", "pri.search.query_failed", "/search/query_failed", Cell::Count),
    ("search.scroll_current", "pri.search.scroll_current", "/search/scroll_current", Cell::Count),
    ("search.scroll_time", "pri.search.scroll_time", "/search/scroll_time_in_millis", Cell::Millis),
    ("search.scroll_total", "pri.search.scroll_total", "/search/scroll_total", Cell::Count),
    (
        "search.point_in_time_current",
        "pri.search.point_in_time_current",
        "/search/point_in_time_current",
        Cell::Count,
    ),
    (
        "search.point_in_time_time",
        "pri.search.point_in_time_time",
        "/search/point_in_time_time_in_millis",
        Cell::Millis,
    ),
    (
        "search.point_in_time_total",
        "pri.search.point_in_time_total",
        "/search/point_in_time_total",
        Cell::Count,
    ),
    ("segments.count", "pri.segments.count", "/segments/count", Cell::Count),
    ("segments.memory", "pri.segments.memory", "/segments/memory_in_bytes", Cell::Bytes),
    (
        "segments.index_writer_memory",
        "pri.segments.index_writer_memory",
        "/segments/index_writer_memory_in_bytes",
        Cell::Bytes,
    ),
    (
        "segments.version_map_memory",
        "pri.segments.version_map_memory",
        "/segments/version_map_memory_in_bytes",
        Cell::Bytes,
    ),
    (
        "segments.fixed_bitset_memory",
        "pri.segments.fixed_bitset_memory",
        "/segments/fixed_bit_set_memory_in_bytes",
        Cell::Bytes,
    ),
    ("warmer.current", "pri.warmer.current", "/warmer/current", Cell::Count),
    ("warmer.total", "pri.warmer.total", "/warmer/total", Cell::Count),
    ("warmer.total_time", "pri.warmer.total_time", "/warmer/total_time_in_millis", Cell::Millis),
    ("suggest.current", "pri.suggest.current", "/search/suggest_current", Cell::Count),
    ("suggest.time", "pri.suggest.time", "/search/suggest_time_in_millis", Cell::Millis),
    ("suggest.total", "pri.suggest.total", "/search/suggest_total", Cell::Count),
];

/// The statistics columns of one `_cat/indices` row; blank for an index with
/// no statistics here -- a closed one, or one held by another node.
fn index_stat_columns(stats: Option<&Value>, unit: Option<&str>) -> Vec<(&'static str, String)> {
    let mut out = Vec::with_capacity(INDEX_STAT_COLS.len() * 2 + 4);
    let num = |ptr: &str| stats.and_then(|s| s.pointer(ptr)).and_then(|v| v.as_u64());
    for (name, pri, ptr, cell) in INDEX_STAT_COLS {
        let text = num(ptr)
            .map(|v| match cell {
                Cell::Count => v.to_string(),
                Cell::Bytes => crate::api::shared::sized(unit, v),
                Cell::Millis => crate::api::shared::time_value_text(v * 1_000_000),
            })
            .unwrap_or_default();
        out.push((*name, text.clone()));
        out.push((*pri, text));
    }
    let memory = num("/segments/memory_in_bytes").map(|v| crate::api::shared::sized(unit, v));
    out.push(("memory.total", memory.clone().unwrap_or_default()));
    out.push(("pri.memory.total", memory.unwrap_or_default()));
    out.push(("search.throttled", if stats.is_some() { "false".into() } else { String::new() }));
    let last = num("/indexing/max_last_index_request_timestamp").filter(|v| *v > 0);
    out.push(("last_index_request_timestamp", last.map(|v| v.to_string()).unwrap_or_default()));
    out
}

pub async fn cat_indices(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    if p.contains_key("help") {
        let mut all: Vec<&str> = CAT_INDEX_COLS.to_vec();
        for (name, pri, _, _) in INDEX_STAT_COLS {
            all.push(name);
            all.push(pri);
        }
        all.extend(["memory.total", "pri.memory.total", "search.throttled"]);
        all.push("last_index_request_timestamp");
        return cat_help(&all);
    }
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
    let names = if expr.is_empty() {
        crate::api::cluster_names(&store)
    } else {
        crate::api::cluster_resolve(&store, &expr)
    };
    // a name given outright must resolve to something -- it may be an alias,
    // whose own name never appears among the indices it stands for
    if !expr.is_empty() && !ignore_unavailable(&p) {
        for part in expr.split(',').map(|n| n.trim()).filter(|n| !n.contains('*')) {
            if crate::api::cluster_resolve(&store, part).is_empty() {
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
    // `system` keeps the table to system indices or away from them, and
    // asking for them sweeps in the hidden ones they are unless the request
    // names its own wildcards -- OpenSearch 3.9's filter: a system index
    // created hidden appeared under `*tasks-*` only with `system=true`, and
    // not at all under `system=true&expand_wildcards=open`
    let system = p.get("system").map(|v| v != "false");
    let show_hidden = named_outright
        || asked_for_hidden
        || dot_pattern
        || (system == Some(true) && !p.contains_key("expand_wildcards"));
    let mut rows = Vec::new();
    // the columns read off an index's statistics are worked out only when a
    // caller names its columns; the default table does not show them
    let wants_stats = p.get("h").map(|h| !h.is_empty()).unwrap_or(false);
    // `bytes` asks for the sizes as plain numbers in the unit it names
    let unit = p.get("bytes").map(|s| s.to_string());
    let sized = |bytes: u64| crate::api::shared::sized(unit.as_deref(), bytes);
    let published = crate::cluster::current_state();
    // On a cluster the node holding an index's primary is the one that can
    // say how many documents it has and what it takes on disk, so that node
    // writes the row. The node the request reached writes the rows for the
    // indices no node holds; every row is gathered into one table.
    let clustered = published.nodes.len() > 1;
    let me = crate::cluster::identity().id.clone();
    let primary_here = |n: &str| {
        published.routing.shards_of(n).any(|c| {
            c.primary
                && c.node.as_ref() == Some(&me)
                && matches!(
                    c.state,
                    crate::cluster::state::ShardState::Started
                        | crate::cluster::state::ShardState::Relocating
                )
        })
    };
    let held_somewhere = |n: &str| {
        published.routing.shards_of(n).any(|c| {
            c.primary
                && matches!(
                    c.state,
                    crate::cluster::state::ShardState::Started
                        | crate::cluster::state::ShardState::Relocating
                )
        })
    };
    // rows for indices no node holds are written once, by the node the
    // request reached rather than by every node answering it
    let forwarded = crate::cluster::forward::answering_forward();
    for n in names {
        if clustered {
            if held_somewhere(&n) {
                if !primary_here(&n) {
                    continue;
                }
            } else if forwarded {
                continue;
            }
        }
        let Some(st) = store.get(&n) else {
            // A node alone holds every index there is, so one the published
            // state names and this node does not hold is one on its way out:
            // the backing indices of a data stream just deleted were listed
            // here for a moment while `GET` already answered 404.
            if !clustered {
                continue;
            }
            // an index of the cluster whose copies are on other nodes: what
            // the manager published is what there is to say about it here
            let Some(m) = published.indices.get(&n) else { continue };
            let hidden = m
                .settings
                .pointer("/index/hidden")
                .map(|v| v == "true" || v == true)
                .unwrap_or(false);
            if !show_hidden && hidden {
                continue;
            }
            let only = vec![n.clone()];
            let health = published.health_status(Some(&only));
            if p.get("health").map(|h| h != health).unwrap_or(false) {
                continue;
            }
            rows.push(vec![
                ("health", health.to_string()),
                ("status", "open".to_string()),
                ("index", n.clone()),
                ("uuid", m.uuid.clone()),
                ("pri", m.number_of_shards.to_string()),
                ("rep", m.number_of_replicas.to_string()),
                ("docs.count", "0".to_string()),
                ("docs.deleted", "0".to_string()),
                ("store.size", sized(0)),
                ("pri.store.size", sized(0)),
                ("creation.date", "0".to_string()),
                ("creation.date.string", String::new()),
            ]);
            if wants_stats && let Some(row) = rows.last_mut() {
                row.extend(index_stat_columns(None, unit.as_deref()));
            }
            continue;
        };
        let g = st.read();
        if !show_hidden && g.setting("hidden").map(|v| v == "true").unwrap_or(false) {
            continue;
        }
        let described = crate::api::system_index_description(&g.name);
        if system.is_some_and(|wanted| wanted != described.is_some()) {
            continue;
        }
        // health is the cluster's answer about the index, not this node's
        // share of it: a copy held here says nothing about the copy elsewhere
        let only = vec![g.name.clone()];
        let health = match published.indices.get(&g.name) {
            Some(_) => published.health_status(Some(&only)).to_string(),
            // no published state (a node running alone before the coordinator
            // has started): an index asking for replicas will not get them
            None if g.numeric_setting("number_of_replicas").unwrap_or(0) > 0 => "yellow".into(),
            None => "green".to_string(),
        };
        if p.get("health").map(|h| h != &health).unwrap_or(false) {
            continue;
        }
        // a closed index has no shard open to count, so those columns are
        // blank rather than zero
        let searcher = g.reader.searcher();
        let docs = searcher.num_docs();
        // what `_stats` and `_cat/segments` count as deleted: the documents
        // still in a segment that a merge has not yet rewritten
        let deleted: u64 =
            searcher.segment_readers().iter().map(|r| r.num_deleted_docs() as u64).sum();
        let bytes_on_disk = store.index_size(&g.name);
        let count = |v: String| if g.closed { String::new() } else { v };
        let mut row = vec![
            ("health", health),
            ("status", if g.closed { "close".into() } else { "open".to_string() }),
            ("index", g.name.clone()),
            ("uuid", g.uuid.clone()),
            // what the index was asked for, not what one node can give it
            ("pri", g.numeric_setting("number_of_shards").unwrap_or(1).to_string()),
            ("rep", g.numeric_setting("number_of_replicas").unwrap_or(0).to_string()),
            ("docs.count", count(docs.to_string())),
            ("docs.deleted", count(deleted.to_string())),
            ("store.size", count(sized(bytes_on_disk))),
            ("pri.store.size", count(sized(bytes_on_disk))),
            // when the index was made, as the epoch and as text
            ("creation.date", g.created_millis().to_string()),
            ("creation.date.string", g.created_string()),
            ("system", described.is_some().to_string()),
            ("system.description", described.unwrap_or_default().to_string()),
        ];
        if wants_stats {
            let stats = (!g.closed).then(|| {
                crate::api::index_stats(&g, bytes_on_disk, None, &p, Some(&store.request_cache))
            });
            row.extend(index_stat_columns(stats.as_ref(), unit.as_deref()));
        }
        rows.push(row);
    }
    rows.sort_by(|a, b| a[2].1.cmp(&b[2].1));
    // the system columns are in the default table only when the request
    // asked about system indices at all
    let mut defaults = vec![
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
    ];
    if system.is_some() {
        defaults.extend(["system", "system.description"]);
    }
    let rows = cat_only_default(rows, &defaults, &p);
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
    use crate::cluster::state::ShardState;
    // how many copies each node holds, as the manager placed them, and how
    // many wait for a node; the path names which nodes to describe
    let live = crate::cluster::current_state();
    let only: Option<Vec<String>> = node
        .as_ref()
        .map(|Path(w)| w.split(',').map(|x| x.trim().to_string()).collect())
        .filter(|v: &Vec<String>| !v.iter().any(|x| matches!(x.as_str(), "_all" | "*")));
    let me = crate::cluster::identity();
    let wanted = |n: &crate::cluster::state::DiscoveryNode| -> bool {
        match &only {
            None => true,
            Some(o) => o.iter().any(|x| {
                *x == n.name
                    || *x == n.id.as_str()
                    || (x == "_local" && n.id == me.id)
                    || ((x == "_master" || x == "_cluster_manager")
                        && live.cluster_manager.as_ref() == Some(&n.id))
                    || (x.contains('*') && crate::store::glob_match(x, &n.name))
            }),
        }
    };
    // `bytes` asks for the sizes as plain numbers in that unit rather than as
    // text a person would read
    let unit = p.get("bytes").map(|s| s.to_string());
    let size = |bytes: u64| crate::api::shared::sized(unit.as_deref(), bytes);
    // the disk this node keeps its data on, measured; another node's disk is
    // its own to report, and is left blank rather than made up
    let disk =
        crate::api::sysinfo::disk(&store.data_dir().map(|d| d.to_path_buf()).unwrap_or_else(
            || std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")),
        ));
    let on_disk: u64 = store.names().iter().map(|n| store.index_size(n)).sum();
    let mut rows: Vec<Vec<(&str, String)>> = Vec::new();
    for n in live.nodes.values().filter(|n| n.is_data() && wanted(n)) {
        let count =
            live.routing.on_node(&n.id).filter(|c| c.state != ShardState::Unassigned).count();
        let ip = n.transport_address.split(':').next().unwrap_or("").to_string();
        let (total, avail) = match (&disk, n.id == me.id) {
            (Some(d), true) => (d.total, d.available),
            _ => (0, 0),
        };
        let known = |v: String| if total > 0 { v } else { String::new() };
        rows.push(vec![
            ("shards", count.to_string()),
            ("disk.indices", known(size(on_disk))),
            ("disk.used", known(size(total.saturating_sub(avail)))),
            ("disk.avail", known(size(avail))),
            ("disk.total", known(size(total))),
            ("disk.percent", known(crate::api::sysinfo::percent(total - avail, total).to_string())),
            ("host", ip.clone()),
            ("ip", ip),
            ("node", n.name.clone()),
        ]);
    }
    let unassigned = live.routing.all().filter(|c| c.state == ShardState::Unassigned).count();
    if unassigned > 0 && only.is_none() {
        rows.push(vec![
            ("shards", unassigned.to_string()),
            ("disk.indices", String::new()),
            ("disk.used", String::new()),
            ("disk.avail", String::new()),
            ("disk.total", String::new()),
            ("disk.percent", String::new()),
            ("host", String::new()),
            ("ip", String::new()),
            ("node", "UNASSIGNED".to_string()),
        ]);
    }
    cat_render_cols(CAT_ALLOCATION_COLS, rows, &p)
}

/// `_cat/nodeattrs` -- the attributes a node was started with.
pub async fn cat_nodeattrs(Query(p): Query<Params>) -> Response {
    // every node of the cluster and what it says about itself: the attributes
    // it was configured with (`node.attr.*`), and the ones the engine adds
    let live = crate::cluster::current_state();
    let me = crate::cluster::identity();
    let mut rows: Vec<Vec<(&str, String)>> = Vec::new();
    let nodes: Vec<(String, String, String, std::collections::BTreeMap<String, String>)> =
        if live.nodes.is_empty() {
            vec![(
                me.name.clone(),
                me.id.as_str().to_string(),
                me.transport_address.clone(),
                me.attributes
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                    .collect(),
            )]
        } else {
            live.nodes
                .iter()
                .map(|(id, n)| {
                    (
                        n.name.clone(),
                        id.as_str().to_string(),
                        n.transport_address.clone(),
                        n.attributes.clone(),
                    )
                })
                .collect()
        };
    for (name, id, address, attrs) in nodes {
        let ip = address.split(':').next().unwrap_or("127.0.0.1").to_string();
        let port = address.split(':').nth(1).unwrap_or("9300").to_string();
        let mut all: Vec<(String, String)> = attrs.into_iter().collect();
        for (k, v) in node_attrs() {
            if !all.iter().any(|(x, _)| *x == k) {
                all.push((k, v));
            }
        }
        for (attr, value) in all {
            rows.push(vec![
                ("node", name.clone()),
                ("id", id.clone()),
                ("pid", std::process::id().to_string()),
                ("host", ip.clone()),
                ("ip", ip.clone()),
                ("port", port.clone()),
                ("attr", attr),
                ("value", value),
            ]);
        }
    }
    let rows = cat_only_default(rows, &["node", "host", "ip", "attr", "value"], &p);
    cat_render_cols(CAT_NODEATTRS_COLS, rows, &p)
}

/// `_cat/plugins` -- nothing is loaded, so the table is empty.
/// `_cat/plugins` -- what OpenSearch would need a plugin installed for, and
/// this engine has built in.
///
/// They are reported because a client that asks whether it may use `icu_
/// tokenizer` deserves a true answer, and the answer is yes. It does mean a
/// suite written to check that its own plugin is the *only* one installed
/// cannot pass here, which is a property of a single binary rather than of a
/// missing feature.
pub async fn cat_plugins(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    let me = crate::cluster::identity();
    let (id, node) = (me.id.to_string(), me.name.to_string());
    let version = env!("CARGO_PKG_VERSION");
    let _ = &store;
    let built_in: &[(&str, &str)] = &[
        ("analysis-icu", "The ICU analysis plugin integrates the Lucene ICU module"),
        // the three that read a script rather than splitting on spaces are
        // the dictionaries, and a build without them answers `kuromoji`,
        // `nori` and `smartcn` with an error: a client told the plugin is
        // here would be told a falsehood
        #[cfg(feature = "cjk")]
        ("analysis-kuromoji", "The Japanese (kuromoji) analysis plugin"),
        #[cfg(feature = "cjk")]
        ("analysis-nori", "The Korean (nori) analysis plugin"),
        ("analysis-phonenumber", "The phone number analysis plugin"),
        ("analysis-phonetic", "The Phonetic Analysis plugin"),
        #[cfg(feature = "cjk")]
        ("analysis-smartcn", "Smart Chinese analysis plugin"),
        ("analysis-stempel", "The Stempel (Polish) analysis plugin"),
        ("analysis-ukrainian", "The Ukrainian analysis plugin"),
        ("ingest-user-agent", "Ingest processor that parses user agent strings"),
        (
            "ingest-geoip",
            "Ingest processor that adds information about the geographical \
                          location of ip addresses",
        ),
        ("lang-painless", "An easy, safe and fast scripting language for OpenSearch"),
        ("lang-expression", "Lucene expressions integration for OpenSearch"),
        ("lang-mustache", "Mustache scripting integration for OpenSearch"),
        ("opensearch-index-management", "OpenSearch Index Management Plugin"),
        ("opensearch-knn", "OpenSearch k-NN plugin"),
        ("opensearch-security", "Provide access control related features for OpenSearch"),
        ("opensearch-sql", "OpenSearch SQL"),
        (
            "repository-azure",
            "The Azure Repository plugin adds support for Azure storage \
                             repositories",
        ),
        (
            "repository-gcs",
            "The GCS repository plugin adds Google Cloud Storage support for \
                           repositories",
        ),
        ("repository-s3", "The S3 repository plugin adds S3 repositories"),
    ];
    let rows: Vec<Vec<(&str, String)>> = built_in
        .iter()
        .map(|(name, description)| {
            vec![
                ("id", id.clone()),
                ("name", node.clone()),
                ("component", (*name).to_string()),
                ("version", version.to_string()),
                ("description", (*description).to_string()),
            ]
        })
        .collect();
    cat_render_cols(CAT_PLUGINS_COLS, rows, &p)
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
        // what the pool has done, counted as requests pass through it; see
        // `pools` for what each column means on this node
        let counted = crate::api::pools::POOLS.iter().find(|x| x.name == *name);
        let of = |f: fn(&crate::api::pools::Pool) -> u64| counted.map(f).unwrap_or(0).to_string();
        let me = crate::cluster::identity();
        let (host, port) =
            me.transport_address.rsplit_once(':').unwrap_or((me.host.as_str(), "9300"));
        rows.push(vec![
            ("node_name", me.name.clone()),
            ("node_id", me.id.as_str().to_string()),
            ("id", me.id.as_str().to_string()),
            ("pid", std::process::id().to_string()),
            ("host", host.to_string()),
            ("ip", host.to_string()),
            ("port", port.to_string()),
            ("ephemeral_node_id", me.ephemeral_id.as_str().to_string()),
            ("name", name.to_string()),
            ("type", kind.to_string()),
            ("active", of(crate::api::pools::Pool::active)),
            ("pool_size", of(crate::api::pools::Pool::threads)),
            ("size", of(crate::api::pools::Pool::size)),
            ("queue", of(crate::api::pools::Pool::queue)),
            ("queue_size", "-1".to_string()),
            ("rejected", of(crate::api::pools::Pool::rejected)),
            ("largest", of(crate::api::pools::Pool::largest)),
            ("completed", of(crate::api::pools::Pool::completed)),
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

/// `_cat/tasks` -- every task running on the node, the request asking among
/// them.
pub async fn cat_tasks(headers: axum::http::HeaderMap, Query(p): Query<Params>) -> Response {
    let _me = crate::tasks::register(crate::tasks::NewTask {
        action: "cluster:monitor/tasks/lists",
        description: String::new(),
        cancellable: false,
        parent: None,
        headers: crate::tasks::headers_of(&headers),
    });
    let me = crate::cluster::identity();
    let rows: Vec<Vec<(&str, String)>> = crate::tasks::running()
        .iter()
        .map(|task| {
            let start = task.start_millis;
            let clock = start / 1000 % 86_400;
            let text = |s: &str| if s.is_empty() { "-".to_string() } else { s.to_string() };
            // the header a caller tags its request with comes back on the
            // task, which is how they find their own among everyone's
            let opaque = task.headers.get("X-Opaque-Id").and_then(|v| v.as_str()).unwrap_or("");
            vec![
                ("action", task.action.clone()),
                ("task_id", task.name()),
                (
                    "parent_task_id",
                    task.parent
                        .map(|n| format!("{}:{n}", me.id.as_str()))
                        .unwrap_or_else(|| "-".to_string()),
                ),
                ("type", "transport".to_string()),
                ("start_time", start.to_string()),
                (
                    "timestamp",
                    format!("{:02}:{:02}:{:02}", clock / 3600, clock / 60 % 60, clock % 60),
                ),
                ("running_time", crate::tasks::time_text(task.running_nanos())),
                ("ip", me.host.clone()),
                ("node", me.name.clone()),
                ("description", text(&task.description)),
                ("x_opaque_id", text(opaque)),
            ]
        })
        .collect();
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
    let rows = cat_only_default(rows, &defaults, &p);
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
    // every index of the cluster, and the aliases it carries: an alias belongs
    // to the index, wherever its copies are
    let published = crate::cluster::current_state();
    for n in crate::api::cluster_names(&store) {
        let held: std::collections::BTreeMap<String, Value> = match store.get(&n) {
            Some(st) => st.read().aliases.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            None => published
                .indices
                .get(&n)
                .and_then(|m| m.aliases.as_object().cloned())
                .map(|o| o.into_iter().collect())
                .unwrap_or_default(),
        };
        for (a, def) in &held {
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
            let index_hidden = match store.get(&n) {
                Some(st) => st.read().setting("hidden").map(|v| v == "true").unwrap_or(false),
                None => published
                    .indices
                    .get(&n)
                    .and_then(|m| m.settings.pointer("/index/hidden"))
                    .map(|v| v == "true" || v == true)
                    .unwrap_or(false),
            };
            let hidden =
                def.get("is_hidden").and_then(|v| v.as_bool()).unwrap_or(false) || index_hidden;
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
    // the same answer `/_cluster/health` gives, in a table: a row that said
    // one node and green whatever the cluster was doing is the first thing
    // an operator looks at and the last thing that should be made up
    let health = crate::api::cluster::cluster_health(
        axum::extract::State(store.clone()),
        None,
        axum::extract::Query(Params::new()),
    )
    .await;
    let (_, health) = crate::api::error_parts_or_body(health).await;
    let text = |key: &str, fallback: &str| -> String {
        match health.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => fallback.to_string(),
        }
    };
    let percent = health
        .get("active_shards_percent_as_number")
        .and_then(|v| v.as_f64())
        .map(|f| format!("{f:.1}%"))
        .unwrap_or_else(|| "0.0%".into());
    // the clock the row is read at, as the reference writes it
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let clock = format!("{:02}:{:02}:{:02}", now / 3600 % 24, now / 60 % 60, now % 60);
    let mut row: Vec<(&str, String)> = vec![
        ("epoch", now.to_string()),
        ("timestamp", clock),
        ("cluster", text("cluster_name", "boostsearch")),
        ("status", text("status", "red")),
        ("node.total", text("number_of_nodes", "0")),
        ("node.data", text("number_of_data_nodes", "0")),
        ("discovered_cluster_manager", text("discovered_cluster_manager", "false")),
        ("shards", text("active_shards", "0")),
        ("pri", text("active_primary_shards", "0")),
        ("relo", text("relocating_shards", "0")),
        ("init", text("initializing_shards", "0")),
        ("unassign", text("unassigned_shards", "0")),
        ("pending_tasks", text("number_of_pending_tasks", "0")),
        ("max_task_wait_time", "-".into()),
        ("active_shards_percent", percent),
    ];
    // `ts=false` drops the two time columns, leaving the cluster's own state
    if p.get("ts").map(|v| v == "false").unwrap_or(false) {
        row.retain(|(k, _)| *k != "epoch" && *k != "timestamp");
    }
    cat_render_cols(CAT_HEALTH_COLS, vec![row], &p)
}
