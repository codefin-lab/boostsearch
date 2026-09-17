//! `_plugins/_performanceanalyzer` -- whether the metric collectors are on.
//!
//! The plugin the reference ships writes a stream of per-shard and per-thread
//! metrics to disk for an out-of-process analyser to read. Nothing here does
//! that: there is no collector to switch on, no writer thread and no metrics
//! directory. What is answered is the read side of the switches, reporting
//! them off, which is what the reference reports on a node where the
//! collectors were never enabled. The numbers are the settings' own
//! defaults, not a measurement of anything collected.

use super::*;

/// The retention the plugin defaults its batch metrics to, in minutes.
const BATCH_RETENTION_MINUTES: u32 = 7;

/// `GET _plugins/_performanceanalyzer/config` and the per-feature forms of it
/// -- what this node has switched on.
///
/// The plugin answers the same body on `config`, `rca/config`,
/// `logging/config`, `batch/config` and `threadContentionMonitoring/config`:
/// one read reports every switch, and the path only says which of them the
/// caller came to change.
pub async fn config(Query(p): Query<Params>) -> Response {
    respond(
        &p,
        json!({
            "performanceAnalyzerEnabled": false,
            "rcaEnabled": false,
            "loggingEnabled": false,
            "shardsPerCollection": 0,
            "batchMetricsEnabled": false,
            "threadContentionMonitoringEnabled": false,
            "batchMetricsRetentionPeriodMinutes": BATCH_RETENTION_MINUTES,
        }),
    )
}

/// `GET _plugins/_performanceanalyzer/cluster/config` and its per-feature
/// forms -- the same switches as the cluster holds them.
///
/// `currentPerformanceAnalyzerClusterState` is a bitmask of which features
/// the cluster has turned on; zero is none of them.
pub async fn cluster_config(Query(p): Query<Params>) -> Response {
    respond(
        &p,
        json!({
            "currentPerformanceAnalyzerClusterState": 0,
            "shardsPerCollection": 0,
            "collectorsSetting": 0,
            "batchMetricsRetentionPeriodMinutes": BATCH_RETENTION_MINUTES,
        }),
    )
}

/// `GET _plugins/_performanceanalyzer/override/cluster/config` -- the
/// root-cause analysis units an operator has turned on or off by hand.
///
/// The plugin answers the overrides as a JSON string rather than as JSON, and
/// a client that parses the field expects that; with no analysis running here
/// there is nothing to override, so every list is empty.
pub async fn override_cluster_config(Query(p): Query<Params>) -> Response {
    let empty = json!({"rcas": [], "deciders": [], "actions": [], "collectors": []});
    let overrides = json!({"enable": empty, "disable": empty});
    respond(&p, json!({"overrides": overrides.to_string()}))
}
