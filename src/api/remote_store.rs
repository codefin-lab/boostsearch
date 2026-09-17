//! `_remotestore` -- the segment store a shard can be backed by.
//!
//! OpenSearch can keep a shard's segments and translog in a repository and
//! recover a shard from there rather than from a peer. Nothing here is backed
//! that way: a shard's files are this node's, and there is no remote store to
//! read them back from. Both endpoints still answer the way a node with no
//! remote store configured answers -- the reference itself is such a node, and
//! these were compared against it -- so a client that asks is told there is
//! nothing rather than that the API does not exist.

use super::*;

/// `POST /_remotestore/_restore` -- recover the named indices from their
/// remote store.
///
/// The body names the indices. With no remote store behind them there are no
/// files to read back, so the answer counts what would have been recovered
/// and the indices are left exactly as they are. The reference, asked the
/// same with no remote store configured, answers the same envelope and leaves
/// the index red, having scheduled a recovery from a store that holds
/// nothing; leaving the data alone is the same answer without the damage.
pub async fn restore_remote_store(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let asked: Value = match serde_json::from_str(body.trim()) {
        Ok(v) => v,
        Err(_) => Value::Null,
    };
    // `indices` takes one name or a list of them
    let named: Vec<String> = match asked.get("indices") {
        Some(Value::String(s)) => s.split(',').map(|n| n.trim().to_string()).collect(),
        Some(Value::Array(a)) => {
            a.iter().filter_map(|v| v.as_str()).map(|s| s.to_string()).collect()
        }
        _ => Vec::new(),
    };
    let named: Vec<String> = named.into_iter().filter(|n| !n.is_empty()).collect();
    if named.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: indices are missing;",
        );
    }
    // the names are resolved leniently: a name or a pattern that reaches no
    // index is passed over rather than refused
    let mut found: Vec<String> = Vec::new();
    for part in &named {
        for name in store.resolve(part) {
            if !found.contains(&name) {
                found.push(name);
            }
        }
    }
    let all_shards = flag(&p, "restore_all_shards");
    let mut shards = 0u64;
    for name in &found {
        let Some(st) = store.get(name) else { continue };
        let g = st.read();
        // recovering every shard replaces the index, and an open index
        // cannot be replaced under a client that is reading it
        if all_shards && !g.closed {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "illegal_state_exception",
                format!(
                    "cannot restore index [{name}] because an open index with same name/uuid \
                     already exists in the cluster. Close the existing index."
                ),
            );
        }
        shards += g.shard_count().max(1);
    }
    respond(
        &p,
        json!({"remote_store": {
            "snapshot": "remote_store",
            "indices": found,
            "shards": {"total": shards, "failed": 0, "successful": shards},
        }}),
    )
}

/// `GET /_remotestore/stats/{index}` and `/{shard_id}` -- what each shard has
/// uploaded to and downloaded from its remote store.
///
/// A shard with no remote store behind it reports nothing, so the answer
/// counts no shards, as the reference's answer for such an index does. The
/// index must exist all the same: naming one that does not is the same
/// mistake here as anywhere.
pub async fn remote_store_stats(
    State(store): State<Store>,
    path: Path<(String,)>,
    Query(p): Query<Params>,
) -> Response {
    let Path((index,)) = path;
    if store.resolve(&index).is_empty()
        && !crate::cluster::current_state().indices.contains_key(&index)
    {
        return no_such_index(&index);
    }
    respond(
        &p,
        json!({
            "_shards": {"total": 0, "successful": 0, "failed": 0},
            "indices": {},
        }),
    )
}

/// The same, with a shard named: the shard narrows what is reported, and
/// there is nothing to report either way.
pub async fn remote_store_stats_shard(
    state: State<Store>,
    Path((index, _shard)): Path<(String, String)>,
    p: Query<Params>,
) -> Response {
    remote_store_stats(state, Path((index,)), p).await
}
