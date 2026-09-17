//! `_plugins/_replication` -- the cross-cluster replication counters.
//!
//! This is replication *between clusters*: a follower index that pulls the
//! operations of a leader index in another cluster, and the auto-follow rules
//! that start one when a leader matching a pattern appears. Nothing here does
//! that -- the replication this engine runs is between the shards of one
//! cluster, which `_cat/shards` and `_cluster/health` report on -- so no
//! index is following or being followed and every counter is zero. They are
//! not zero because the work has been idle; there is no such work.

use super::*;

/// `GET _plugins/_replication/autofollow_stats` -- what the auto-follow rules
/// have started.
pub async fn autofollow_stats(Query(p): Query<Params>) -> Response {
    respond(
        &p,
        json!({
            "num_success_start_replication": 0,
            "num_failed_start_replication": 0,
            "num_failed_leader_calls": 0,
            "failed_indices": [],
            "autofollow_stats": [],
        }),
    )
}

/// `GET _plugins/_replication/follower_stats` -- what this cluster has pulled
/// as a follower.
pub async fn follower_stats(Query(p): Query<Params>) -> Response {
    respond(
        &p,
        json!({
            "num_syncing_indices": 0,
            "num_bootstrapping_indices": 0,
            "num_paused_indices": 0,
            "num_failed_indices": 0,
            "num_shard_tasks": 0,
            "num_index_tasks": 0,
            "operations_written": 0,
            "operations_read": 0,
            "failed_read_requests": 0,
            "throttled_read_requests": 0,
            "failed_write_requests": 0,
            "throttled_write_requests": 0,
            "follower_checkpoint": 0,
            "leader_checkpoint": 0,
            "total_write_time_millis": 0,
            "index_stats": {},
        }),
    )
}

/// `GET _plugins/_replication/leader_stats` -- what followers have read from
/// this cluster.
pub async fn leader_stats(Query(p): Query<Params>) -> Response {
    respond(
        &p,
        json!({
            "num_replicated_indices": 0,
            "operations_read": 0,
            "translog_size_bytes": 0,
            "operations_read_lucene": 0,
            "operations_read_translog": 0,
            "total_read_time_lucene_millis": 0,
            "total_read_time_translog_millis": 0,
            "bytes_read": 0,
            "index_stats": {},
        }),
    )
}
