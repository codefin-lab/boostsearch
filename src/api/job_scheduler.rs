//! `_plugins/_job_scheduler` -- the jobs the scheduler is holding.
//!
//! Every scheduled job on this node belongs to index management: an index
//! under a policy, a transform or a rollup, all of them kept in the index
//! management configuration index and all of them swept by the loops the
//! server starts. So this lists what is really scheduled, read from that
//! index, under the job type the plugin registers them as.

use super::*;

/// The job type index management registers its jobs with the scheduler as.
const JOB_TYPE: &str = "opendistro-index-management";

/// A time as the scheduler prints one, or `none` where it has none to print.
fn at(ms: Option<i64>) -> Value {
    match ms.filter(|ms| *ms > 0) {
        Some(ms) => json!(crate::cluster::state::iso_millis(ms as u64)),
        None => json!("none"),
    }
}

/// A schedule as the scheduler's own read surface prints it: the stored form
/// is the plugin's (`interval` or `cron` with its own fields), and this is the
/// flattened shape with the type named.
fn schedule_of(schedule: &Value) -> Value {
    if let Some(interval) = schedule.get("interval") {
        return json!({
            "type": "interval",
            "start_time": at(interval.get("start_time").and_then(|v| v.as_i64())),
            "interval": interval.get("period").and_then(|v| v.as_i64()).unwrap_or(0),
            "unit": interval.get("unit").and_then(|v| v.as_str()).unwrap_or("Minutes"),
            "delay": at(interval.get("schedule_delay").and_then(|v| v.as_i64())),
        });
    }
    if let Some(cron) = schedule.get("cron") {
        return json!({
            "type": "cron",
            "expression": cron.get("expression").and_then(|v| v.as_str()).unwrap_or(""),
            "timezone": cron.get("timezone").and_then(|v| v.as_str()).unwrap_or(""),
            "delay": at(cron.get("schedule_delay").and_then(|v| v.as_i64())),
        });
    }
    Value::Null
}

/// One job, as the scheduler reports it.
fn job(
    id: &str,
    name: &str,
    enabled: bool,
    enabled_at: Option<i64>,
    updated_at: Option<i64>,
    schedule: Value,
    next: Option<i64>,
) -> Value {
    json!({
        "job_type": JOB_TYPE,
        "job_id": id,
        "index_name": crate::ism::CONFIG_INDEX,
        "name": name,
        // a job is descheduled when it has been switched off; it stays in the
        // configuration index either way
        "descheduled": !enabled,
        "enabled": enabled,
        "enabled_time": at(enabled_at),
        "last_update_time": at(updated_at),
        // the sweeper does not write down when it last ran a job, so there is
        // no run time to report -- only when the next one is due
        "last_execution_time": "none",
        "last_expected_execution_time": "none",
        "next_expected_execution_time": at(next),
        "schedule": schedule,
        // the sweeps run on the cluster manager alone and one at a time, so a
        // job is never held under a lock and never has its start jittered to
        // spread it away from another node's
        "lock_duration": 0,
        "jitter": 0.0,
    })
}

/// `GET _plugins/_job_scheduler/api/jobs` -- every job this node sweeps.
pub async fn jobs(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    let mut out: Vec<Value> = Vec::new();
    // an index under a policy is looked at on the index management interval
    // rather than on a schedule of its own, so that interval is its schedule
    let interval_ms = crate::ism::job_interval_ms(&store) as i64;
    let sweep_next = crate::ism::engine::last_sweep_millis().map(|at| at + interval_ms);
    for (id, body) in crate::ism::all(&store, "managed_index") {
        let managed = &body["managed_index"];
        let enabled_at = managed.get("enabled_time").and_then(|v| v.as_i64());
        out.push(job(
            &id,
            managed.get("index").and_then(|v| v.as_str()).unwrap_or_default(),
            managed.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
            enabled_at,
            managed.get("last_updated_time").and_then(|v| v.as_i64()),
            json!({
                "type": "interval",
                "start_time": at(enabled_at),
                "interval": interval_ms,
                "unit": "Millis",
                "delay": "none",
            }),
            sweep_next,
        ));
    }
    for (kind, enabled_field, updated_field) in
        [("transform", "enabled_at", "updated_at"), ("rollup", "enabled_time", "last_updated_time")]
    {
        for (id, held) in crate::ism::jobs::all(&store, kind) {
            let held = &held.body[kind];
            out.push(job(
                &id,
                &id,
                held.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
                held.get(enabled_field).and_then(|v| v.as_i64()),
                held.get(updated_field).and_then(|v| v.as_i64()),
                schedule_of(&held["schedule"]),
                crate::ism::jobs::next_due(kind, &id),
            ));
        }
    }
    let total = out.len();
    respond(&p, json!({"jobs": out, "failures": [], "total_jobs": total}))
}

/// `GET _plugins/_job_scheduler/api/locks` -- the locks jobs are holding.
///
/// The plugin takes a lock document per job so that two nodes do not run the
/// same one at once. The sweeps here run on the cluster manager alone, one
/// job at a time, so nothing needs a lock and none is ever written.
pub async fn locks(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"total_locks": 0, "locks": {}}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_schedule_is_read_back_flattened() {
        let interval = schedule_of(&json!({
            "interval": {"start_time": 1_000, "period": 5, "unit": "Minutes"}
        }));
        assert_eq!(interval["type"], "interval");
        assert_eq!(interval["interval"], 5);
        assert_eq!(interval["unit"], "Minutes");
        assert_eq!(interval["start_time"], "1970-01-01T00:00:01.000Z");
        // a job with no delay says so rather than reporting a delay of zero
        assert_eq!(interval["delay"], "none");
        let cron = schedule_of(&json!({"cron": {"expression": "0 * * * *", "timezone": "UTC"}}));
        assert_eq!(cron["type"], "cron");
        assert_eq!(cron["expression"], "0 * * * *");
        assert_eq!(cron["timezone"], "UTC");
    }
}
