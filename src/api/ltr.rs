//! `_plugins/_ltr` -- the learning-to-rank feature stores.
//!
//! A feature store is an index of features, feature sets and trained models
//! that a rescoring query draws on, and the plugin caches what it has loaded
//! out of one. There is no feature store on this node and no store index to
//! find, so there is nothing cached: the counts are the empty ones, not a
//! measurement of a cache that happens to be cold.

use super::*;

/// `GET _plugins/_ltr/stats` -- the caches, per node, and the stores.
pub async fn stats(Query(p): Query<Params>) -> Response {
    let me = crate::cluster::identity();
    let empty_cache = json!({
        "eviction_count": 0,
        "miss_count": 0,
        "entry_count": 0,
        "memory_usage_in_bytes": 0,
        "hit_count": 0,
    });
    respond(
        &p,
        json!({
            "_nodes": {"total": 1, "successful": 1, "failed": 0},
            "cluster_name": me.cluster_name,
            // a store would be listed here with the health of its index; with
            // no store there is nothing whose health could be anything but
            // green
            "stores": {},
            "status": "green",
            "nodes": {
                me.id.as_str(): {
                    "cache": {
                        "feature": empty_cache,
                        "featureset": empty_cache,
                        "model": empty_cache,
                    },
                    "request_total_count": 0,
                    "request_error_count": 0,
                }
            }
        }),
    )
}
