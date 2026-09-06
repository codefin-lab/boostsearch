//! `_plugins/_knn` -- what the vector side of the engine is holding.

use super::*;

/// `GET _plugins/_knn/stats` -- how much vector work this node has done.
pub async fn stats(
    State(store): State<Store>,
    _scope: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let me = crate::cluster::identity();
    let mut vectors = 0usize;
    let mut indices = 0usize;
    for name in store.names() {
        let Some(st) = store.get(&name) else { continue };
        let g = st.read();
        if g.mapping.vector_fields.is_empty() {
            continue;
        }
        indices += 1;
        vectors += g.vectors.read().len();
    }
    respond(
        &p,
        json!({
            "cluster_name": crate::cluster::identity().cluster_name,
            "circuit_breaker_triggered": false,
            "model_index_status": Value::Null,
            "nodes": {
                me.id.as_str(): {
                    // the vectors are read where they are rather than loaded
                    // into a cache of their own, so nothing is ever waiting
                    // to be warmed and nothing is ever evicted
                    "graph_memory_usage": 0,
                    "graph_memory_usage_percentage": 0.0,
                    "graph_index_requests": 0,
                    "graph_index_errors": 0,
                    "graph_query_requests": 0,
                    "graph_query_errors": 0,
                    "knn_query_requests": 0,
                    "cache_capacity_reached": false,
                    "load_success_count": 0,
                    "load_exception_count": 0,
                    "total_load_time": 0,
                    "eviction_count": 0,
                    "hit_count": 0,
                    "miss_count": 0,
                    "indices_in_cache": {},
                    "script_compilations": 0,
                    "script_compilation_errors": 0,
                    "script_query_requests": 0,
                    "script_query_errors": 0,
                    "indexing_from_model_degraded": false,
                    "vector_count": vectors,
                    "vector_indices": indices,
                }
            }
        }),
    )
}

/// `GET _plugins/_knn/warmup/{index}` -- read an index's vectors in, so that
/// the first search does not pay for it.
///
/// Here that means making sure the table is built, which it already is unless
/// the index was opened and never searched.
pub async fn warmup(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_else(|| "*".into());
    for name in store.resolve(&expr) {
        let Some(st) = store.get(&name) else { continue };
        let needs = {
            let g = st.read();
            !g.mapping.vector_fields.is_empty() && g.vectors.read().is_empty()
        };
        if needs {
            st.write().rebuild_vectors();
        }
    }
    respond(&p, json!({"_shards": {"total": 1, "successful": 1, "failed": 0}}))
}
