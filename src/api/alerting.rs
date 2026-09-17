//! `_plugins/_alerting` -- the state of the scheduled-job sweeper.
//!
//! The reference's alerting plugin keeps monitors in a configuration index
//! and a sweeper that comes round to run the ones that are due. There are no
//! monitors here and no alerting configuration index, so nothing is listed;
//! what is real is the sweeper itself, because index management runs one, and
//! the sweep times reported are that sweeper's own.

use super::*;

/// `GET _plugins/_alerting/stats` -- whether the jobs are being swept.
pub async fn stats(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    let me = crate::cluster::identity();
    let now = crate::store::now_millis();
    // how long ago the sweeper last came round, which is what the field
    // counts: the plugin reports the age of the last pass, not its duration
    let since = crate::ism::engine::last_sweep_millis().map(|at| (now - at).max(0));
    // a pass is late once it has taken longer than two intervals; one interval
    // is the time it is supposed to take between passes, so a single slow
    // pass is not a sweeper that has stopped
    let interval = crate::ism::job_interval_ms(&store) as i64;
    let on_time = since.map(|age| age <= interval * 2).unwrap_or(false);
    let roles: Vec<String> = me.roles.iter().map(|r| r.to_uppercase()).collect();
    // the sweeper's clock is this node's own, so the node answering reports
    // itself rather than asking its peers for a number they hold locally
    respond(
        &p,
        json!({
            "_nodes": {"total": 1, "successful": 1, "failed": 0},
            "cluster_name": me.cluster_name,
            "opendistro.scheduled_jobs.enabled": crate::ism::enabled(&store),
            "plugins.scheduled_jobs.enabled": crate::ism::enabled(&store),
            // the alerting plugin's own configuration index, which is not one
            // this node keeps: its scheduled jobs live in the index management
            // configuration index instead
            "scheduled_job_index_exists": false,
            "scheduled_job_index_status": Value::Null,
            "nodes_on_schedule": u32::from(on_time),
            "nodes_not_on_schedule": u32::from(!on_time),
            "nodes": {
                me.id.as_str(): {
                    "name": me.name,
                    "schedule_status": if on_time { "green" } else { "red" },
                    "roles": roles,
                    "job_scheduling_metrics": {
                        "last_full_sweep_time_millis": since.unwrap_or(0),
                        "full_sweep_on_time": on_time,
                    },
                    // the monitors this node is running, of which there are
                    // none: nothing here registers an alerting job
                    "jobs_info": {},
                }
            }
        }),
    )
}
