//! `_insights` -- the query insights read surface.
//!
//! The reference's plugin keeps a heap of the slowest, hungriest queries of
//! the last window and exports them to an index. No such record is kept here:
//! a search is answered and its cost is counted into the index's search stats
//! (`_stats/search`), not into a per-query ring. So the lists are empty and
//! the heap sizes are zero -- the shape a node answers with when nothing has
//! been collected -- rather than a sample of the queries this node ran.

use super::*;

/// The window a top-queries list would cover, as the setting spells it.
const DEFAULT_WINDOW: &str = "5m";
/// How many queries a top-queries list holds.
const DEFAULT_TOP_N: u64 = 10;
/// Where the plugin writes the queries it collected when nothing says
/// otherwise: an index on the cluster itself.
const DEFAULT_EXPORTER: &str = "local_index";
/// How long an exported record is kept, in days.
const DEFAULT_RETENTION: u64 = 7;
/// What a collected query is grouped by before it is ranked.
const DEFAULT_GROUP_BY: &str = "none";

/// `GET _insights/top_queries` -- the costliest queries of the window.
pub async fn top_queries(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"top_queries": []}))
}

/// `GET _insights/live_queries` -- the searches running right now.
///
/// The plugin reads this from its own record of in-flight searches, which
/// carries the per-query cost measurements it reports alongside each one.
/// Nothing here measures a search while it runs, so nothing is listed; the
/// tasks a search registers are what `_tasks` answers with.
pub async fn live_queries(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"live_queries": []}))
}

/// `GET _insights/health_stats` -- what the collector itself is holding.
///
/// `ThreadPoolInfo` names the executor the plugin runs its exporter on. There
/// is no such executor here, so it is reported empty rather than as a pool
/// the node does not have.
pub async fn health_stats(Query(p): Query<Params>) -> Response {
    let me = crate::cluster::identity();
    let empty_heap =
        json!({"TopQueriesHeapSize": 0, "QueryGroupCount_Total": 0, "QueryGroupCount_MaxHeap": 0});
    respond(
        &p,
        json!({
            me.id.as_str(): {
                "ThreadPoolInfo": {},
                "QueryRecordsQueueSize": 0,
                "TopQueriesHealthStats": {
                    "latency": empty_heap,
                    "cpu": empty_heap,
                    "memory": empty_heap,
                },
                "FieldTypeCacheStats": {
                    "size_in_bytes": 0,
                    "entry_count": 0,
                    "evictions": 0,
                    "hit_count": 0,
                    "miss_count": 0,
                },
            }
        }),
    )
}

/// `GET _insights/settings` -- what the cluster has asked for.
///
/// This reads back cluster settings, so it answers what the settings say and
/// nothing else. Where an operator has written a value it stands; where they
/// have written nothing, the reference's own default for that setting is
/// reported, which is what a settings read means everywhere else on this
/// server. Reporting the collectors off instead would have been this node
/// answering about itself under the name of a setting nobody had touched, and
/// a console that reads this to fill in its form would then show an operator
/// a value they had not set and the reference would not have shown them.
///
/// What the settings ask for is not carried out here: nothing samples a
/// query's latency, CPU or memory, so `_insights/top_queries` stays empty
/// whatever `enabled` says, and no exporter writes a record however
/// `exporter.type` reads. The lists above are the honest part of this
/// surface; this one is the operator's own configuration read back to them.
pub async fn settings(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    let mut out = serde_json::Map::new();
    for metric in ["latency", "cpu", "memory"] {
        let key = |leaf: &str| format!("search.insights.top_queries.{metric}.{leaf}");
        out.insert(
            metric.to_string(),
            json!({
                "enabled": bool_setting(&store, &key("enabled")).unwrap_or(true),
                "top_n_size": u64_setting(&store, &key("top_n_size")).unwrap_or(DEFAULT_TOP_N),
                "window_size": str_setting(&store, &key("window_size"))
                    .unwrap_or_else(|| DEFAULT_WINDOW.to_string()),
            }),
        );
    }
    // the setting sits under `grouping.` even though the body reports it a
    // level up, so this reads the name an operator would have written
    out.insert(
        "grouping".to_string(),
        json!({
            "group_by": str_setting(&store, "search.insights.top_queries.grouping.group_by")
                .unwrap_or_else(|| DEFAULT_GROUP_BY.to_string()),
        }),
    );
    out.insert(
        "exporter".to_string(),
        json!({
            "type": str_setting(&store, "search.insights.top_queries.exporter.type")
                .unwrap_or_else(|| DEFAULT_EXPORTER.to_string()),
            "delete_after_days": u64_setting(
                &store,
                "search.insights.top_queries.exporter.delete_after_days",
            )
            .unwrap_or(DEFAULT_RETENTION),
        }),
    );
    respond(&p, json!({"persistent": Value::Object(out)}))
}

/// A cluster setting written either as a bare value or as the string a
/// settings file spells it with.
fn bool_setting(store: &Store, key: &str) -> Option<bool> {
    store.cluster_setting(key).and_then(|v| v.as_bool().or_else(|| v.as_str().map(|s| s == "true")))
}

fn u64_setting(store: &Store, key: &str) -> Option<u64> {
    store.cluster_setting(key).and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
}

fn str_setting(store: &Store, key: &str) -> Option<String> {
    store.cluster_setting(key).map(|v| match v {
        Value::String(s) => s,
        other => other.to_string(),
    })
}
