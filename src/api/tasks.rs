//! Work the node is doing, and asking it to stop.
//!
//! The tasks themselves are kept in `crate::tasks`; this is what the REST API
//! says about them: a task by id, running or stored in `.tasks`, the list of
//! them, cancelling them, and changing the rate of a walk.

use super::*;
use crate::tasks::{NewTask, Task};
use std::sync::Arc;

/// What a task id says about itself, or why it is not one.
enum Named<'a> {
    /// a node and a number, as every task this node runs is named
    Task(&'a str, u64),
    /// the finished work a resize or an open was reported under, named after
    /// what it did rather than numbered
    Described(&'a str, &'a str),
}

fn named(id: &str) -> std::result::Result<Named<'_>, Response> {
    let Some((node, what)) = id.split_once(':') else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("malformed task id {id}"),
        ));
    };
    if let Some((node, n)) = crate::tasks::parse_id(id) {
        return Ok(Named::Task(node, n));
    }
    if ["open", "shrink", "split", "clone"].iter().any(|k| what.starts_with(k)) {
        return Ok(Named::Described(node, what));
    }
    let number = id.rsplit_once(':').map(|(_, n)| n).unwrap_or(what);
    Err((
        StatusCode::BAD_REQUEST,
        axum::Json(json!({
            "error": {
                "root_cause": [{
                    "type": "illegal_argument_exception", "reason": format!("malformed task id {id}"),
                }],
                "type": "illegal_argument_exception",
                "reason": format!("malformed task id {id}"),
                "caused_by": {
                    "type": "number_format_exception",
                    "reason": format!("For input string: \"{number}\""),
                },
            },
            "status": 400,
        })),
    )
        .into_response())
}

/// Whether a node id names this node.
fn is_me(node: &str) -> bool {
    let me = crate::cluster::identity();
    node == me.id.as_str() || node == me.name
}

/// Whether a node id names any node of the cluster.
fn known_node(node: &str) -> bool {
    is_me(node) || crate::cluster::current_state().nodes.keys().any(|n| n.as_str() == node)
}

/// This node as the tasks API writes it above its tasks.
fn node_section(tasks: serde_json::Map<String, Value>) -> Value {
    let me = crate::cluster::identity();
    let roles = match me.roles.is_empty() {
        true => json!(["cluster_manager", "data", "ingest"]),
        false => json!(me.roles),
    };
    json!({
        "name": me.name, "transport_address": me.transport_address,
        "host": me.host, "ip": me.transport_address,
        "roles": roles, "attributes": me.attributes,
        "tasks": tasks,
    })
}

/// A node failure, as the answer of an action sent to each node lists one.
fn node_failure(node: &str, cause: Value) -> Value {
    json!({
        "type": "failed_node_exception",
        "reason": format!("Failed node [{node}]"),
        "node_id": node,
        "caused_by": cause,
    })
}

/// The answer to an action on one task that did not reach it.
fn failed_on_node(p: &Params, node: &str, id: &str, missing: &str) -> Response {
    let cause = match known_node(node) {
        true => json!({
            "type": "resource_not_found_exception",
            "reason": format!("task [{id}] {missing}"),
        }),
        false => json!({
            "type": "no_such_node_exception",
            "reason": format!("No such node [{node}]"),
            "node_id": node,
        }),
    };
    respond(p, json!({"node_failures": [node_failure(node, cause)], "nodes": {}}))
}

/// How long a request waits for a task to finish, when it was asked to.
fn wait_limit(p: &Params) -> std::time::Duration {
    let ms = p.get("timeout").and_then(|t| crate::tasks::millis_of(t)).unwrap_or(30_000.0);
    std::time::Duration::from_millis(ms.max(0.0) as u64)
}

/// Wait for a task to be done. Answers false if it still runs at the limit.
async fn wait_until_done(id: u64, limit: std::time::Duration) -> bool {
    let until = std::time::Instant::now() + limit;
    while crate::tasks::get(id).is_some() {
        if std::time::Instant::now() >= until {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    true
}

/// `_tasks/{id}` -- what became of a task.
///
/// A running task answers with where it has got to. One that has finished
/// answers with what it kept in `.tasks`, if it was sent off to run there;
/// a task that was waited for kept nothing, and is not found, as in
/// OpenSearch.
pub async fn get_task(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let (node, n) = match named(&id) {
        Ok(Named::Task(node, n)) => (node, n),
        Ok(Named::Described(_, what)) => return described_task(&p, what),
        Err(refusal) => return refusal,
    };
    if is_me(node)
        && let Some(task) = crate::tasks::get(n)
    {
        if !flag(&p, "wait_for_completion") {
            return respond(&p, json!({"completed": false, "task": task.info(true, true)}));
        }
        if !wait_until_done(n, wait_limit(&p)).await {
            return err(
                StatusCode::REQUEST_TIMEOUT,
                "timeout_exception",
                format!("Timed out waiting for completion of [{}]", task.name()),
            );
        }
    }
    let stored = store.get(".tasks").map(|st| crate::api::read_source(&st.read(), &id));
    if let Some(Some(record)) = stored {
        return respond(&p, record);
    }
    let missing = format!("task [{id}] isn't running and hasn't stored its results");
    if !known_node(node) {
        return unknown_node_task(&id, node, &missing);
    }
    match stored {
        // the index is there and the task is not in it
        Some(None) => err(StatusCode::NOT_FOUND, "resource_not_found_exception", missing),
        _ => err_caused_by_status(
            StatusCode::NOT_FOUND,
            "resource_not_found_exception",
            &missing,
            "index_not_found_exception",
            "no such index [.tasks]",
        ),
    }
}

/// `DELETE /_tasks/{id}` -- forget what a finished task kept.
///
/// This takes the record out of `.tasks`; it does not stop anything. A task
/// that is still running is a conflict, and the caller is pointed at the
/// cancel API, as the reference points them. A task with no record is not
/// found, with the same two shapes `GET` uses: a node the cluster does not
/// have says so, and a missing `.tasks` index is named as the cause.
pub async fn delete_task(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let node = match named(&id) {
        Ok(Named::Task(node, n)) => {
            if is_me(node) && crate::tasks::get(n).is_some() {
                return err(
                    StatusCode::CONFLICT,
                    "status_exception",
                    format!(
                        "task [{id}] is still running and cannot be deleted; use the cancel \
                         tasks API to cancel running tasks"
                    ),
                );
            }
            node
        }
        // the work an open or a resize was reported under keeps no record
        Ok(Named::Described(node, _)) => node,
        Err(refusal) => return refusal,
    };
    let missing = format!("task [{id}] isn't running and hasn't stored its results");
    let Some(st) = store.get(".tasks") else {
        if !known_node(node) {
            return unknown_node_task(&id, node, &missing);
        }
        return err_caused_by_status(
            StatusCode::NOT_FOUND,
            "resource_not_found_exception",
            &missing,
            "index_not_found_exception",
            "no such index [.tasks]",
        );
    };
    let held = crate::api::read_source(&st.read(), &id).is_some();
    if !held {
        if !known_node(node) {
            return unknown_node_task(&id, node, &missing);
        }
        return err(StatusCode::NOT_FOUND, "resource_not_found_exception", missing);
    }
    // a record with children under it is deleted after them, so that no
    // child's result is left with no parent to find it by
    if let Some(parent) = crate::tasks::parse_id(&id).map(|(_, n)| n)
        && !crate::tasks::children(parent).is_empty()
    {
        return err(
            StatusCode::CONFLICT,
            "status_exception",
            format!(
                "task [{id}] has running child tasks and cannot be deleted; wait for or cancel \
                 child tasks first"
            ),
        );
    }
    let ops: crate::cluster::replication::Writes = Default::default();
    let gone = {
        let store = store.clone();
        let id = id.clone();
        let noted = ops.clone();
        tokio::task::spawn_blocking(move || {
            crate::cluster::replication::WRITES.sync_scope(noted, || {
                let Some(st) = store.get(".tasks") else { return false };
                let mut g = st.write();
                let (_, status) = crate::api::delete_doc(&mut g, &id);
                let _ = g.sync_translog();
                status.is_success()
            })
        })
        .await
        .unwrap_or(false)
    };
    let recorded = std::mem::take(&mut *ops.lock());
    if !recorded.is_empty() {
        let _ =
            crate::cluster::replication::finish(StatusCode::OK.into_response(), recorded, "").await;
    }
    if !gone {
        return err(StatusCode::NOT_FOUND, "resource_not_found_exception", missing);
    }
    respond(&p, json!({"acknowledged": true}))
}

/// A task named for a node the cluster does not have: the reason says the
/// node is not here, and the cause says there is no record either.
fn unknown_node_task(id: &str, node: &str, missing: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({
            "error": {
                "root_cause": [{"type": "resource_not_found_exception", "reason": missing}],
                "type": "resource_not_found_exception",
                "reason": format!(
                    "task [{id}] belongs to the node [{node}] which isn't part of the cluster \
                     and there is no record of the task"
                ),
                "caused_by": {"type": "resource_not_found_exception", "reason": missing},
            },
            "status": 404,
        })),
    )
        .into_response()
}

/// The finished work an open or a resize sent off was reported under.
fn described_task(p: &Params, what: &str) -> Response {
    let action =
        if what.starts_with("open") { "indices:admin/open" } else { "indices:admin/resize" };
    respond(
        p,
        json!({
            "completed": true,
            "task": {
                "node": crate::cluster::identity().id.as_str(), "id": 1, "type": "transport",
                "action": action,
                "description": what,
                "start_time_in_millis": 0, "running_time_in_nanos": 0, "cancellable": false,
            },
            "response": {"acknowledged": true, "shards_acknowledged": true},
        }),
    )
}

/// Whether a task is one a request's filters name: `actions`, `nodes` and
/// `parent_task_id`, each of which narrows the list when it is given.
fn filtered(p: &Params, task: &Task) -> bool {
    if let Some(actions) = p.get("actions").filter(|a| !a.is_empty())
        && !actions.split(',').any(|a| {
            let a = a.trim();
            a == "*" || crate::store::glob_match(a, &task.action)
        })
    {
        return false;
    }
    if let Some(nodes) = p.get("nodes").filter(|n| !n.is_empty())
        && !nodes.split(',').any(|n| matches!(n.trim(), "_all" | "_local") || is_me(n.trim()))
    {
        return false;
    }
    if let Some(parent) = p.get("parent_task_id").filter(|t| !t.is_empty())
        && crate::tasks::parse_id(parent).map(|(_, n)| Some(n)) != Some(task.parent)
    {
        return false;
    }
    true
}

pub async fn list_tasks(headers: axum::http::HeaderMap, Query(p): Query<Params>) -> Response {
    // the request asking is itself a task, and lists itself
    let _me = crate::tasks::register(NewTask {
        action: "cluster:monitor/tasks/lists",
        description: String::new(),
        cancellable: false,
        parent: None,
        headers: crate::tasks::headers_of(&headers),
    });
    let detailed = flag(&p, "detailed");
    let listed: Vec<Arc<Task>> =
        crate::tasks::running().into_iter().filter(|t| filtered(&p, t)).collect();
    match p.get("group_by").map(|v| v.as_str()) {
        // `none` asks for them in a plain list
        Some("none") => {
            let all: Vec<Value> = listed.iter().map(|t| t.info(detailed, true)).collect();
            respond(&p, json!({"tasks": all}))
        }
        // `parents` puts each task under the one that started it
        Some("parents") => {
            let mut top = serde_json::Map::new();
            for task in &listed {
                let parent_listed = task.parent.map(|n| listed.iter().any(|t| t.id == n));
                if parent_listed == Some(true) {
                    continue;
                }
                let mut info = task.info(detailed, true);
                let children: Vec<Value> = listed
                    .iter()
                    .filter(|t| t.parent == Some(task.id))
                    .map(|t| t.info(detailed, true))
                    .collect();
                if !children.is_empty() {
                    info["children"] = json!(children);
                }
                top.insert(task.name(), info);
            }
            respond(&p, json!({"tasks": top}))
        }
        _ => {
            if listed.is_empty() {
                return respond(&p, json!({"nodes": {}}));
            }
            let tasks: serde_json::Map<String, Value> =
                listed.iter().map(|t| (t.name(), t.info(detailed, true))).collect();
            let me = crate::cluster::identity().id.as_str().to_string();
            respond(&p, json!({"nodes": {me: node_section(tasks)}}))
        }
    }
}

/// `_tasks/_cancel` and `_tasks/{id}/_cancel` -- stop running work.
///
/// A task that is not running is not found, and a node that is not in the
/// cluster is no such node: both are failures of the node the request was
/// sent to, answered 200 with the failure listed, as OpenSearch answers.
pub async fn cancel_tasks(id: Option<Path<String>>, Query(p): Query<Params>) -> Response {
    let wait = flag(&p, "wait_for_completion");
    let chosen: Vec<Arc<Task>> = match &id {
        Some(Path(id)) => {
            let (node, n) = match named(id) {
                Ok(Named::Task(node, n)) => (node, n),
                Ok(Named::Described(node, _)) => {
                    return failed_on_node(&p, node, id, "is not found");
                }
                Err(refusal) => return refusal,
            };
            match crate::tasks::get(n).filter(|_| is_me(node)) {
                Some(task) if !task.cancellable => {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        format!("task [{id}] doesn't support cancellation"),
                    );
                }
                Some(task) => vec![task],
                None => return failed_on_node(&p, node, id, "is not found"),
            }
        }
        None => crate::tasks::running()
            .into_iter()
            .filter(|t| t.cancellable && filtered(&p, t))
            .collect(),
    };
    for task in &chosen {
        crate::tasks::cancel(task);
    }
    if wait {
        for task in &chosen {
            wait_until_done(task.id, wait_limit(&p)).await;
        }
    }
    if chosen.is_empty() {
        return respond(&p, json!({"nodes": {}}));
    }
    let tasks: serde_json::Map<String, Value> =
        chosen.iter().map(|t| (t.name(), t.info(false, true))).collect();
    let me = crate::cluster::identity().id.as_str().to_string();
    respond(&p, json!({"nodes": {me: node_section(tasks)}}))
}

/// `_reindex/{id}/_rethrottle` and its by-query spellings -- a new rate for
/// a running walk, effective as OpenSearch makes it effective: at once when
/// it is faster, from the next batch when it is slower.
pub async fn rethrottle(Path(id): Path<String>, Query(p): Query<Params>) -> Response {
    let Some(written) = p.get("requests_per_second") else {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "requests_per_second is a required parameter",
        );
    };
    let rate = match written.parse::<f64>() {
        Ok(r) if r > 0.0 || r == -1.0 => r,
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                "[requests_per_second] must be a float greater than 0. Use -1 to disable \
                 throttling.",
            );
        }
    };
    let (node, n) = match named(&id) {
        Ok(Named::Task(node, n)) => (node, n),
        Ok(Named::Described(node, _)) => return failed_on_node(&p, node, &id, "is missing"),
        Err(refusal) => return refusal,
    };
    let Some(task) = crate::tasks::get(n).filter(|_| is_me(node)) else {
        return failed_on_node(&p, node, &id, "is missing");
    };
    task.rethrottle(rate);
    let mut tasks = serde_json::Map::new();
    tasks.insert(task.name(), task.info(true, true));
    let me = crate::cluster::identity().id.as_str().to_string();
    respond(&p, json!({"nodes": {me: node_section(tasks)}}))
}

/// The index a finished task's result is kept in, made the way OpenSearch
/// makes it the first time a result is kept: one shard, as many replicas as
/// there are nodes to hold them up to one, and a mapping that takes nothing
/// but a task's result.
fn tasks_index(store: &Store) -> Option<Arc<crate::store::IdxLock>> {
    if let Some(st) = store.get(".tasks") {
        return Some(st);
    }
    let unindexed = json!({"type": "object", "enabled": false});
    let body = json!({
        "settings": {"index": {
            "number_of_shards": 1, "number_of_replicas": 0, "auto_expand_replicas": "0-1",
            "priority": 2147483647,
        }},
        "mappings": {
            "dynamic": "strict",
            "_meta": {"version": 5},
            "properties": {
                "completed": {"type": "boolean"},
                "error": unindexed,
                "response": unindexed,
                "task": {"properties": {
                    "action": {"type": "keyword"},
                    "cancellable": {"type": "boolean"},
                    "cancellation_time_millis": {"type": "long"},
                    "cancelled": {"type": "boolean"},
                    "description": {"type": "text"},
                    "headers": unindexed,
                    "id": {"type": "long"},
                    "node": {"type": "keyword"},
                    "parent_task_id": {"type": "keyword"},
                    "resource_stats": unindexed,
                    "running_time_in_nanos": {"type": "long"},
                    "start_time_in_millis": {"type": "long"},
                    "status": unindexed,
                    "type": {"type": "keyword"},
                }},
            },
        },
    });
    // two tasks finishing at once may both find it missing; the one that
    // loses the race to make it uses the one that won
    let _ = store.create(".tasks", &body);
    store.get(".tasks")
}

/// Keep a finished task's result in `.tasks`, under the task's id.
pub(crate) async fn store_task_result(store: &Store, id: &str, record: Value) {
    let writes: crate::cluster::replication::Writes = Default::default();
    let (store, id, noted) = (store.clone(), id.to_string(), writes.clone());
    let _ = tokio::task::spawn_blocking(move || {
        crate::cluster::replication::WRITES.sync_scope(noted, || {
            let Some(st) = tasks_index(&store) else { return };
            let mut g = st.write();
            if let Err(e) = crate::api::write_doc_internal(&mut g, &id, record, "index", None, None)
            {
                tracing::warn!("the result of task [{id}] could not be kept: {:?}", e.status());
            }
            let _ = g.sync_translog();
        })
    })
    .await;
    let ops = std::mem::take(&mut *writes.lock());
    if !ops.is_empty() {
        let _ = crate::cluster::replication::finish(StatusCode::OK.into_response(), ops, "").await;
    }
}
