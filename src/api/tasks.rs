//! Work the node is doing, and asking it to stop.

use super::*;

/// The part of a walk's answer that is its running status.
///
/// A task reports what it has done so far, which is everything the answer
/// says except how long the request itself took.
fn status_of(answer: &Value) -> Value {
    let mut status = answer.clone();
    if let Some(o) = status.as_object_mut() {
        o.remove("took");
        o.remove("timed_out");
    }
    status
}

/// `_tasks/{id}` -- what became of a task.
///
/// Everything this engine is asked to do finishes before the request returns,
/// so a task named here is one that has already completed.
pub async fn get_task(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    // a walk that was asked not to be waited for left its answer here
    if let Some(answer) = store.task_answer(&id) {
        return respond(
            &p,
            json!({
                "completed": true,
                "task": {
                    "node": "node-0", "id": 1, "type": "transport",
                    "action": "indices:data/write/by_query",
                    "description": id,
                    "start_time_in_millis": 0, "running_time_in_nanos": 0, "cancellable": true,
                    // what the walk did is the task's status as well as its
                    // answer; the status is the tally without the timing
                    "status": status_of(&answer),
                },
                "response": answer,
            }),
        );
    }
    // the id carries what the task was, after the node that ran it
    let node = id.split_once(':').map(|(n, _)| n).unwrap_or("").to_string();
    // a task named after a node that is not here is a task nobody has heard of
    if !node.is_empty() && node != "node-0" && node != "boostsearch" {
        return err(
            StatusCode::NOT_FOUND,
            "resource_not_found_exception",
            format!(
                "task [{id}] belongs to the node [{node}] which isn't part of the cluster and \
                 there is no record of the task"
            ),
        );
    }
    let what = id.split_once(':').map(|(_, d)| d).unwrap_or(&id).to_string();
    let action = if what.starts_with("open") {
        "indices:admin/open"
    } else if what.starts_with("shrink") || what.starts_with("split") || what.starts_with("clone") {
        "indices:admin/resize"
    } else {
        "indices:admin/tasks"
    };
    respond(
        &p,
        json!({
            "completed": true,
            "task": {
                "node": "node-0", "id": 1, "type": "transport",
                "action": action,
                "description": what,
                "start_time_in_millis": 0, "running_time_in_nanos": 0, "cancellable": false,
            },
            "response": {"acknowledged": true, "shards_acknowledged": true},
        }),
    )
}

pub async fn list_tasks(headers: axum::http::HeaderMap, Query(p): Query<Params>) -> Response {
    // the header a caller tags its request with comes back on the task
    let mut task_headers = serde_json::Map::new();
    if let Some(v) = headers.get("x-opaque-id").and_then(|v| v.to_str().ok()) {
        task_headers.insert("X-Opaque-Id".into(), json!(v));
    }
    // the request asking is itself a task, which is the one every caller of
    // this endpoint sees
    let task = json!({
        "node": "node-0", "id": 1, "type": "transport",
        "action": "cluster:monitor/tasks/lists", "start_time_in_millis": 0,
        "running_time_in_nanos": 0, "cancellable": false,
        "headers": Value::Object(task_headers),
        "resource_stats": {
            "average": {"cpu_time_in_nanos": 0, "memory_in_bytes": 0},
            "total": {"cpu_time_in_nanos": 0, "memory_in_bytes": 0},
            "min": {"cpu_time_in_nanos": 0, "memory_in_bytes": 0},
            "max": {"cpu_time_in_nanos": 0, "memory_in_bytes": 0},
            "thread_info": {"thread_executions": 1, "active_threads": 1},
        },
    });
    // `group_by` says how to arrange them: under the node that runs them, or
    // flat when the caller wants to walk parents instead
    // an action filter names the tasks the caller wants to see; the only task
    // running is this listing, and a filter that does not name it lists none
    if let Some(actions) = p.get("actions")
        && !actions.split(',').any(|a| {
            let a = a.trim();
            a == "*" || crate::store::glob_match(a, "cluster:monitor/tasks/lists")
        })
    {
        return match p.get("group_by").map(|v| v.as_str()) {
            Some("none") => respond(&p, json!({"tasks": []})),
            Some("parents") => respond(&p, json!({"tasks": {}})),
            _ => respond(&p, json!({"nodes": {}})),
        };
    }
    match p.get("group_by").map(|v| v.as_str()) {
        // `none` asks for them in a plain list, `parents` keyed by their id
        Some("none") => return respond(&p, json!({"tasks": [task]})),
        Some("parents") => return respond(&p, json!({"tasks": {"node-0:1": task}})),
        _ => {}
    }
    respond(
        &p,
        json!({
            "nodes": {"node-0": {
                "name": "boostsearch", "transport_address": "127.0.0.1:9300",
                "host": "127.0.0.1", "ip": "127.0.0.1",
                "roles": ["cluster_manager", "data", "ingest"],
                "tasks": {"node-0:1": task},
            }},
        }),
    )
}

/// `_tasks/_cancel` -- nothing here runs long enough to be cancelled.
pub async fn cancel_tasks(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"nodes": {}, "node_failures": [], "tasks": []}))
}
