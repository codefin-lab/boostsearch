//! The work this node is doing, by task id.
//!
//! OpenSearch gives every piece of work a task: a number unique on the node,
//! the action it runs, when it started, and whether it may be cancelled. The
//! tasks API lists them, `_cancel` stops them, and a job sent off with
//! `wait_for_completion=false` is followed by its id. A task is registered
//! here while its work runs and is gone when the work is, so what is listed
//! is what is really running.

use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, OnceLock};

/// What a piece of work says of itself while it runs.
pub trait Work: Send + Sync {
    /// The task's `status`, if its kind of work reports one.
    fn status(&self) -> Option<Value>;

    /// A new rate for work that holds itself to one. Work that does not
    /// throttle ignores it.
    fn rethrottle(&self, _requests_per_second: f64) {}
}

/// One task.
pub struct Task {
    pub id: u64,
    pub action: String,
    pub description: String,
    pub start_millis: u64,
    started: std::time::Instant,
    pub cancellable: bool,
    pub parent: Option<u64>,
    pub headers: Map<String, Value>,
    cancelled: AtomicBool,
    cancelled_at: AtomicU64,
    work: OnceLock<Arc<dyn Work>>,
    /// woken when the task is cancelled, so work waiting out a throttle does
    /// not sleep through being told to stop
    pub wake: tokio::sync::Notify,
}

impl Task {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Hand the task the work it reports on.
    pub fn attach(&self, work: Arc<dyn Work>) {
        let _ = self.work.set(work);
    }

    pub fn status(&self) -> Option<Value> {
        self.work.get().and_then(|w| w.status())
    }

    pub fn rethrottle(&self, rate: f64) {
        if let Some(w) = self.work.get() {
            w.rethrottle(rate);
        }
    }

    /// The task id as a client writes it: the node, a colon, the number.
    pub fn name(&self) -> String {
        format!("{}:{}", node_id(), self.id)
    }

    pub fn running_nanos(&self) -> u64 {
        self.started.elapsed().as_nanos() as u64
    }

    /// The task as the tasks API shows it.
    ///
    /// A listing shows the status and description only when asked for the
    /// detail; `_tasks/<id>`, a rethrottle and a stored result always carry
    /// them. `resource_stats` belongs to a task that is still running.
    pub fn info(&self, detailed: bool, running: bool) -> Value {
        let mut o = Map::new();
        o.insert("node".into(), json!(node_id()));
        o.insert("id".into(), json!(self.id));
        o.insert("type".into(), json!("transport"));
        o.insert("action".into(), json!(self.action));
        if detailed {
            if let Some(status) = self.status() {
                o.insert("status".into(), status);
            }
            o.insert("description".into(), json!(self.description));
        }
        o.insert("start_time_in_millis".into(), json!(self.start_millis));
        o.insert("running_time_in_nanos".into(), json!(self.running_nanos()));
        o.insert("cancellable".into(), json!(self.cancellable));
        if self.cancellable {
            o.insert("cancelled".into(), json!(self.is_cancelled()));
        }
        if let Some(parent) = self.parent {
            o.insert("parent_task_id".into(), json!(format!("{}:{parent}", node_id())));
        }
        o.insert("headers".into(), Value::Object(self.headers.clone()));
        if detailed && running {
            o.insert("resource_stats".into(), idle_resource_stats());
        }
        if self.is_cancelled() {
            o.insert(
                "cancellation_time_millis".into(),
                json!(self.cancelled_at.load(Ordering::Relaxed)),
            );
        }
        Value::Object(o)
    }
}

/// The resource figures a running task carries. This node does not measure
/// CPU or memory per task, and the reference reports zeros for work that is
/// not a search, so zeros are what is said.
pub fn idle_resource_stats() -> Value {
    let zero = json!({"cpu_time_in_nanos": 0, "memory_in_bytes": 0});
    json!({
        "average": zero, "total": zero, "min": zero, "max": zero,
        "thread_info": {"thread_executions": 0, "active_threads": 0},
    })
}

/// This node's id, which is the first half of every task id it hands out.
pub fn node_id() -> String {
    crate::cluster::identity().id.as_str().to_string()
}

static NEXT_ID: AtomicU64 = AtomicU64::new(0);
static RUNNING: LazyLock<Mutex<BTreeMap<u64, Arc<Task>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// The registration of a running task. Dropping it takes the task off the
/// list, however the work ended.
pub struct Registered(pub Arc<Task>);

impl Drop for Registered {
    fn drop(&mut self) {
        RUNNING.lock().remove(&self.0.id);
    }
}

impl std::ops::Deref for Registered {
    type Target = Arc<Task>;
    fn deref(&self) -> &Arc<Task> {
        &self.0
    }
}

/// What a new task is.
pub struct NewTask<'a> {
    pub action: &'a str,
    pub description: String,
    pub cancellable: bool,
    pub parent: Option<u64>,
    pub headers: Map<String, Value>,
}

/// Put a task on the list.
pub fn register(new: NewTask<'_>) -> Registered {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed) + 1;
    let task = Arc::new(Task {
        id,
        action: new.action.to_string(),
        description: new.description,
        start_millis: crate::store::now_millis().max(0) as u64,
        started: std::time::Instant::now(),
        cancellable: new.cancellable,
        parent: new.parent,
        headers: new.headers,
        cancelled: AtomicBool::new(false),
        cancelled_at: AtomicU64::new(0),
        work: OnceLock::new(),
        wake: tokio::sync::Notify::new(),
    });
    // a parent cancelled before this child was made has its children
    // cancelled with it
    if let Some(parent) = new.parent
        && get(parent).map(|p| p.is_cancelled()).unwrap_or(false)
    {
        mark_cancelled(&task);
    }
    RUNNING.lock().insert(id, task.clone());
    Registered(task)
}

pub fn get(id: u64) -> Option<Arc<Task>> {
    RUNNING.lock().get(&id).cloned()
}

/// Every running task, oldest first.
pub fn running() -> Vec<Arc<Task>> {
    RUNNING.lock().values().cloned().collect()
}

/// The running tasks whose parent is this one.
pub fn children(id: u64) -> Vec<Arc<Task>> {
    RUNNING.lock().values().filter(|t| t.parent == Some(id)).cloned().collect()
}

fn mark_cancelled(task: &Task) {
    if !task.cancelled.swap(true, Ordering::Relaxed) {
        task.cancelled_at.store(crate::store::now_millis().max(0) as u64, Ordering::Relaxed);
    }
    task.wake.notify_waiters();
    task.wake.notify_one();
}

/// Cancel a task and everything it started.
pub fn cancel(task: &Task) {
    mark_cancelled(task);
    for child in children(task.id) {
        cancel(&child);
    }
}

/// The headers a task carries from the request that started it: the ones
/// OpenSearch copies onto tasks, which is the opaque id a caller tags its
/// requests with.
pub fn headers_of(headers: &axum::http::HeaderMap) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(v) = headers.get("x-opaque-id").and_then(|v| v.to_str().ok()) {
        out.insert("X-Opaque-Id".into(), json!(v));
    }
    out
}

/// A task id split into the node and the number, if it is shaped as one.
pub fn parse_id(written: &str) -> Option<(&str, u64)> {
    let (node, n) = written.rsplit_once(':')?;
    if node.is_empty() {
        return None;
    }
    Some((node, n.parse::<u64>().ok()?))
}

/// A length of time a request names, `30s` or `1m`, in milliseconds; a bare
/// number is milliseconds already.
pub fn millis_of(text: &str) -> Option<f64> {
    match text.trim().parse::<f64>() {
        Ok(ms) => Some(ms),
        Err(_) => crate::search::extras::parse_time_amount(text).map(|nanos| nanos / 1e6),
    }
}

/// A length of time as OpenSearch writes one for people: the largest unit it
/// reaches, with one decimal kept only when it is not zero -- `999.9ms`,
/// `1.5s`, `214micros`, `0s`.
pub fn time_text(nanos: u64) -> String {
    const UNITS: [(u64, &str); 6] = [
        (86_400_000_000_000, "d"),
        (3_600_000_000_000, "h"),
        (60_000_000_000, "m"),
        (1_000_000_000, "s"),
        (1_000_000, "ms"),
        (1_000, "micros"),
    ];
    if nanos == 0 {
        return "0s".to_string();
    }
    let (size, unit) =
        UNITS.iter().copied().find(|(size, _)| nanos >= *size).unwrap_or((1, "nanos"));
    let whole = nanos / size;
    // the decimal is cut, not rounded, as Java's formatting of it cuts
    let tenth = (nanos % size) * 10 / size;
    match tenth {
        0 => format!("{whole}{unit}"),
        t => format!("{whole}.{t}{unit}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_written_as_opensearch_writes_them() {
        assert_eq!(time_text(0), "0s");
        assert_eq!(time_text(999_912_345), "999.9ms");
        assert_eq!(time_text(1_999_000_000), "1.9s");
        assert_eq!(time_text(214_000), "214micros");
        assert_eq!(time_text(2_000_000_000), "2s");
        assert_eq!(time_text(12), "12nanos");
    }

    #[test]
    fn a_task_id_is_a_node_and_a_number() {
        assert_eq!(parse_id("abc:12"), Some(("abc", 12)));
        assert_eq!(parse_id("abc"), None);
        assert_eq!(parse_id("abc:x"), None);
        assert_eq!(parse_id(":1"), None);
    }
}
