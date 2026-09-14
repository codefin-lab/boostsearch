//! Asynchronous search: a search sent off to run, and read back by id.
//!
//! The asynchronous-search plugin answers a submitted search at once if it
//! finishes within `wait_for_completion_timeout`, and otherwise with an id
//! the caller comes back with. A search that finishes after its submit
//! answered is kept only if `keep_on_completion` asked for it, until its
//! `keep_alive` runs out; a failed one is not kept at all, which is the
//! plugin's default of not persisting failures. The states a caller sees are
//! the plugin's: `RUNNING` while it runs, `PERSISTING` in the answer that
//! finds it done and kept, `STORE_RESIDENT` when it is read back afterwards,
//! and `CLOSED` for a search that was answered and let go.

use super::*;
use std::collections::HashMap as Map;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

/// Where a search has got to.
enum Outcome {
    Running,
    Succeeded(Value),
    Failed(Value),
}

/// One submitted search.
struct Submitted {
    id: String,
    start_millis: u64,
    expiration_millis: AtomicU64,
    keep_on_completion: bool,
    /// the caller who submitted it: an id is not a capability anyone who
    /// learns it may spend, and another caller is told it does not exist
    owner: Option<String>,
    shards: u64,
    /// the outcome, and whether the submit request has already answered --
    /// held under one lock, so the search finishing and the submit giving up
    /// on waiting agree on which of them lets the search go
    state: parking_lot::Mutex<(Outcome, bool)>,
    /// set when a finished search is kept to be read back
    kept: AtomicBool,
    task: Arc<crate::tasks::Task>,
    done: tokio::sync::Notify,
}

static SEARCHES: LazyLock<parking_lot::Mutex<Map<String, Arc<Submitted>>>> =
    LazyLock::new(|| parking_lot::Mutex::new(Map::new()));

/// What `_plugins/_asynchronous_search/stats` counts.
#[derive(Default)]
struct Counts {
    submitted: AtomicU64,
    initialized: AtomicU64,
    persisted: AtomicU64,
    search_failed: AtomicU64,
    search_completed: AtomicU64,
    rejected: AtomicU64,
    persist_failed: AtomicU64,
    cancelled: AtomicU64,
}

static COUNTS: LazyLock<Counts> = LazyLock::new(Counts::default);
static SEQ: AtomicU64 = AtomicU64::new(0);

/// The longest a submit may wait, and the longest a result may be kept for,
/// where the cluster settings do not say otherwise.
const MAX_WAIT_MS: f64 = 60_000.0;
const MAX_KEEP_ALIVE_MS: f64 = 5.0 * 86_400_000.0;
const DEFAULT_KEEP_ALIVE_MS: f64 = 86_400_000.0;

fn now_millis() -> u64 {
    crate::store::now_millis().max(0) as u64
}

/// A duration setting of the plugin, read from the cluster settings under
/// either of the names the plugin answers to.
fn plugin_limit(store: &Store, key: &str, default: f64) -> (f64, String) {
    for prefix in ["plugins", "opendistro"] {
        let name = format!("{prefix}.asynchronous_search.{key}");
        if let Some(text) = store.cluster_setting(&name).and_then(|v| v.as_str().map(String::from))
            && let Some(ms) = crate::tasks::millis_of(&text)
        {
            return (ms, text);
        }
    }
    let text = match key {
        "max_wait_for_completion_timeout" => "1m",
        _ => "5d",
    };
    (default, text.to_string())
}

/// The plugin's id: the node, the search's context, a random part and a
/// sequence number, each written after its length, and the whole in base64.
fn new_id(context: u64) -> String {
    use base64::Engine;
    let node = crate::tasks::node_id();
    let context = context.to_string();
    let random: String = crate::store::random_token().chars().take(20).collect();
    let seq = (SEQ.fetch_add(1, Ordering::Relaxed) + 1).to_string();
    let mut bytes = Vec::new();
    for part in [node.as_str(), context.as_str(), random.as_str(), seq.as_str()] {
        bytes.push(part.len() as u8);
        bytes.extend_from_slice(part.as_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Let go of searches whose time has run out, stopping any still running.
fn sweep() {
    let now = now_millis();
    let mut held = SEARCHES.lock();
    held.retain(|_, s| {
        let alive = s.expiration_millis.load(Ordering::Relaxed) > now;
        if !alive {
            crate::tasks::cancel(&s.task);
        }
        alive
    });
}

fn not_found(id: &str) -> Response {
    err(
        StatusCode::NOT_FOUND,
        "resource_not_found_exception",
        format!("Either the resource [{id}] does not exist or you do not have access"),
    )
}

/// The search a caller may see under this id, if there is one.
fn visible(id: &str) -> Option<Arc<Submitted>> {
    sweep();
    let found = SEARCHES.lock().get(id).cloned()?;
    let caller = crate::store::current_owner();
    match (&found.owner, &caller) {
        (Some(owner), Some(caller)) if owner != caller => None,
        _ => Some(found),
    }
}

/// The answer for a search as it stands.
fn answer_for(s: &Submitted, state: &str, outcome: &Outcome, p: &Params) -> Response {
    let mut o = serde_json::Map::new();
    o.insert("id".into(), json!(s.id));
    o.insert("state".into(), json!(state));
    o.insert("start_time_in_millis".into(), json!(s.start_millis));
    o.insert(
        "expiration_time_in_millis".into(),
        json!(s.expiration_millis.load(Ordering::Relaxed)),
    );
    match outcome {
        Outcome::Running => {
            // what a search that has not reached its shards yet can say
            o.insert(
                "response".into(),
                json!({
                    "took": 0, "timed_out": false, "num_reduce_phases": 0,
                    "_shards": {"total": s.shards, "successful": 0, "skipped": 0, "failed": 0},
                    "hits": {"max_score": null, "hits": []},
                }),
            );
        }
        Outcome::Succeeded(response) => {
            o.insert("response".into(), response.clone());
        }
        Outcome::Failed(error) => {
            o.insert("error".into(), error.clone());
        }
    }
    respond(p, Value::Object(o))
}

/// Settle a search that has finished and whose submit has answered: keep it
/// to be read back, or let it go.
fn settle(s: &Submitted, outcome: &Outcome) {
    match outcome {
        Outcome::Running => {}
        Outcome::Succeeded(_) if s.keep_on_completion => {
            s.kept.store(true, Ordering::Relaxed);
            COUNTS.persisted.fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            SEARCHES.lock().remove(&s.id);
        }
    }
}

/// A duration named in the request, in milliseconds, or a refusal of it.
fn millis_param(p: &Params, key: &str) -> std::result::Result<Option<(f64, String)>, Response> {
    let Some(text) = p.get(key) else { return Ok(None) };
    match crate::tasks::millis_of(text) {
        Some(ms) => Ok(Some((ms, text.clone()))),
        None => Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("failed to parse setting [{key}] with value [{text}] as a time value"),
        )),
    }
}

/// `POST _plugins/_asynchronous_search` -- run a search, answering when it is
/// done or when the caller's wait runs out, whichever is first.
pub async fn submit_async_search(
    State(store): State<Store>,
    Query(p): Query<Params>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if p.contains_key("scroll") {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: scrolls are not supported;",
        );
    }
    let keep_alive = match millis_param(&p, "keep_alive") {
        Ok(v) => v,
        Err(refusal) => return refusal,
    };
    if let Some((ms, text)) = &keep_alive
        && *ms < 60_000.0
    {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            format!(
                "Validation Failed: 1: [keep_alive] must be greater than 1 minute, got: {text};"
            ),
        );
    }
    let (max_keep, max_keep_text) = plugin_limit(&store, "max_keep_alive", MAX_KEEP_ALIVE_MS);
    if let Some((ms, _)) = &keep_alive
        && *ms > max_keep
    {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "Keep alive for asynchronous search ({}) is too large. It must be less than \
                 ({max_keep_text}).This limit can be set by changing the \
                 [plugins.asynchronous_search.max_keep_alive] cluster level setting.",
                *ms as u64
            ),
        );
    }
    let wait = match millis_param(&p, "wait_for_completion_timeout") {
        Ok(v) => v.map(|(ms, _)| ms).unwrap_or(1_000.0),
        Err(refusal) => return refusal,
    };
    let (max_wait, max_wait_text) =
        plugin_limit(&store, "max_wait_for_completion_timeout", MAX_WAIT_MS);
    if wait > max_wait {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "Wait for completion timeout for asynchronous search ({}) is too large. It must \
                 be less than ({max_wait_text}).This limit can be set by changing the \
                 [plugins.asynchronous_search.max_wait_for_completion_timeout] cluster level \
                 setting.",
                wait as u64
            ),
        );
    }
    let index = p.get("index").cloned().filter(|i| !i.is_empty());
    // the search is judged as the search it is: the route names no index for
    // the security layer to judge, so the index in the query string is
    // judged here
    if let Some(why) = crate::security::item_refusal(
        &store,
        &["indices:data/read/search"],
        &crate::security::layer::indices_for_expr(&store, index.as_deref().unwrap_or("_all")),
    ) {
        return err(StatusCode::FORBIDDEN, "security_exception", why);
    }
    // a request refused before it was a search is not counted as one
    COUNTS.submitted.fetch_add(1, Ordering::Relaxed);
    COUNTS.initialized.fetch_add(1, Ordering::Relaxed);
    sweep();
    let task = crate::tasks::register(crate::tasks::NewTask {
        action: "indices:data/read/search",
        description: format!(
            "indices[{}], search_type[QUERY_THEN_FETCH]",
            index.as_deref().unwrap_or("")
        ),
        cancellable: true,
        parent: None,
        headers: crate::tasks::headers_of(&headers),
    });
    let shards: u64 = store
        .resolve(index.as_deref().unwrap_or("_all"))
        .iter()
        .filter_map(|n| store.get(n))
        .map(|st| st.read().shard_count())
        .sum();
    let start = now_millis();
    let keep_ms = keep_alive.map(|(ms, _)| ms).unwrap_or(DEFAULT_KEEP_ALIVE_MS);
    let search = Arc::new(Submitted {
        id: new_id(task.id),
        start_millis: start,
        expiration_millis: AtomicU64::new(start + keep_ms as u64),
        keep_on_completion: flag(&p, "keep_on_completion"),
        owner: crate::store::current_owner(),
        shards,
        state: parking_lot::Mutex::new((Outcome::Running, false)),
        kept: AtomicBool::new(false),
        task: task.0.clone(),
        done: tokio::sync::Notify::new(),
    });
    SEARCHES.lock().insert(search.id.clone(), search.clone());

    // the search itself is the ordinary one, with the plugin's own parameters
    // taken out, run as the caller on a thread of its own
    let mut params = p.clone();
    for own in ["index", "keep_alive", "keep_on_completion", "wait_for_completion_timeout"] {
        params.remove(own);
    }
    let caller = crate::security::layer::current_caller();
    let running = search.clone();
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let searched = handle.block_on(async move {
            let run = crate::api::search(State(store), index.map(Path), Query(params), body);
            let response = match caller {
                Some(c) => crate::security::layer::CALLER.scope(c, run).await,
                None => run.await,
            };
            let status = response.status();
            let bytes =
                axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap_or_default();
            let parsed: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
            (status, parsed)
        });
        let (status, parsed) = searched;
        let outcome = match status.is_success() {
            true => {
                COUNTS.search_completed.fetch_add(1, Ordering::Relaxed);
                Outcome::Succeeded(parsed)
            }
            false => {
                COUNTS.search_failed.fetch_add(1, Ordering::Relaxed);
                Outcome::Failed(parsed.get("error").cloned().unwrap_or(parsed))
            }
        };
        let mut state = running.state.lock();
        state.0 = outcome;
        if state.1 {
            settle(&running, &state.0);
        }
        drop(state);
        running.done.notify_waiters();
        drop(task);
    });

    let finished = search.done.notified();
    tokio::pin!(finished);
    finished.as_mut().enable();
    if matches!(search.state.lock().0, Outcome::Running) {
        let _ =
            tokio::time::timeout(std::time::Duration::from_millis(wait.max(0.0) as u64), finished)
                .await;
    }
    let mut state = search.state.lock();
    state.1 = true;
    let label = match &state.0 {
        Outcome::Running => "RUNNING",
        Outcome::Succeeded(_) if search.keep_on_completion => "PERSISTING",
        _ => "CLOSED",
    };
    settle(&search, &state.0);
    answer_for(&search, label, &state.0, &p)
}

/// `GET _plugins/_asynchronous_search/{id}` -- a search read back.
pub async fn get_async_search(Path(id): Path<String>, Query(p): Query<Params>) -> Response {
    let Some(search) = visible(&id) else { return not_found(&id) };
    // reading a search back may give it longer to live
    match millis_param(&p, "keep_alive") {
        Ok(Some((ms, _))) => {
            search.expiration_millis.store(now_millis() + ms as u64, Ordering::Relaxed);
        }
        Ok(None) => {}
        Err(refusal) => return refusal,
    }
    let state = search.state.lock();
    let label = match (&state.0, search.kept.load(Ordering::Relaxed)) {
        (Outcome::Running, _) => "RUNNING",
        (Outcome::Succeeded(_), true) => "STORE_RESIDENT",
        (Outcome::Succeeded(_), false) => "SUCCEEDED",
        (Outcome::Failed(_), _) => "FAILED",
    };
    answer_for(&search, label, &state.0, &p)
}

/// `DELETE _plugins/_asynchronous_search/{id}` -- stop a search, or forget one
/// that was kept.
pub async fn delete_async_search(Path(id): Path<String>, Query(p): Query<Params>) -> Response {
    let Some(search) = visible(&id) else { return not_found(&id) };
    SEARCHES.lock().remove(&id);
    if matches!(search.state.lock().0, Outcome::Running) {
        crate::tasks::cancel(&search.task);
        COUNTS.cancelled.fetch_add(1, Ordering::Relaxed);
    }
    respond(&p, json!({"acknowledged": true}))
}

/// `GET _plugins/_asynchronous_search/stats` -- what the node has done with
/// asynchronous searches since it started.
pub async fn async_search_stats(Query(p): Query<Params>) -> Response {
    let running =
        SEARCHES.lock().values().filter(|s| matches!(s.state.lock().0, Outcome::Running)).count();
    let me = crate::cluster::identity();
    let c = &*COUNTS;
    let n = |a: &AtomicU64| a.load(Ordering::Relaxed);
    respond(
        &p,
        json!({
            "_nodes": {"total": 1, "successful": 1, "failed": 0},
            "cluster_name": me.cluster_name,
            "nodes": {me.id.as_str(): {"asynchronous_search_stats": {
                "submitted": n(&c.submitted),
                "initialized": n(&c.initialized),
                "running_current": running,
                "persisted": n(&c.persisted),
                "search_failed": n(&c.search_failed),
                "search_completed": n(&c.search_completed),
                "rejected": n(&c.rejected),
                "persist_failed": n(&c.persist_failed),
                "cancelled": n(&c.cancelled),
            }}},
        }),
    )
}
