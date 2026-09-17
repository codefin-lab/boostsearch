//! Asynchronous search: a search sent off to run, and read back by id.
//!
//! The asynchronous-search plugin answers a submitted search at once if it
//! finishes within `wait_for_completion_timeout`, and otherwise with an id
//! the caller comes back with. A search that finishes after its submit
//! answered is kept only if `keep_on_completion` asked for it, until its
//! `keep_alive` runs out; a failed one is kept only when
//! `plugins.asynchronous_search.persist_search_failures` says so, which it
//! does not by default. The states a caller sees are the plugin's: `RUNNING`
//! while it runs, `PERSISTING` in the answer that finds it done and kept,
//! `STORE_RESIDENT` when it is read back afterwards, `PERSIST_FAILED` when it
//! could not be kept, and `CLOSED` for a search that was answered and let go.
//!
//! A search runs on the node it was submitted to, and its id names that node.
//! A result that is kept is written to that node's data directory, so it is
//! still there after the node restarts, and it is no longer held in memory
//! once written. A read or a delete that reaches another node is sent on to
//! the node the id names.
//!
//! Nothing here is unbounded: a node runs at most
//! `plugins.asynchronous_search.node_concurrent_running_searches` searches,
//! one caller at most `user_concurrent_running_searches` of them, and the
//! results kept add up to at most `node_retained_bytes` on a node and
//! `user_retained_bytes` for one caller. A submit past any of those is refused
//! with 429, and results whose `keep_alive` ran out are let go on a schedule
//! rather than when somebody next asks.

use super::*;
use axum::http::Uri;
use std::collections::HashMap as Map;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};

/// Where a search has got to.
enum Outcome {
    Running,
    Succeeded(Value),
    Failed(Value),
}

/// One submitted search, while it runs and until its submit has answered.
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
    /// whether the search is still running: read under the map's lock, where
    /// the limits are counted, without taking each search's own lock
    running: AtomicBool,
    /// what became of keeping the result, once the search finished
    kept: parking_lot::Mutex<Keeping>,
    /// set when the search is deleted, so a search finishing after its
    /// delete is not kept
    deleted: AtomicBool,
    task: Arc<crate::tasks::Task>,
    done: tokio::sync::Notify,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Keeping {
    NotAsked,
    Kept,
    Failed,
}

static SEARCHES: LazyLock<parking_lot::Mutex<Map<String, Arc<Submitted>>>> =
    LazyLock::new(|| parking_lot::Mutex::new(Map::new()));

/// A result that was kept: what is needed to count it, find it and let it go.
/// The response itself is on disk, or here when the node keeps nothing on disk.
struct Kept {
    owner: Option<String>,
    expiration_millis: u64,
    bytes: u64,
    held: Option<Arc<Value>>,
}

static KEPT: LazyLock<parking_lot::Mutex<Map<String, Kept>>> =
    LazyLock::new(|| parking_lot::Mutex::new(Map::new()));

/// The callers, and the node, whose last result did not fit in the bytes
/// allowed, with the allowance it did not fit in. Their next submit asking to
/// keep a result is refused until a result of theirs is let go or the
/// allowance is raised: the bytes in use never reach the allowance when every
/// result that would cross it is turned away, so counting them alone would
/// never refuse anyone.
#[derive(Default)]
struct Full {
    node: Option<u64>,
    users: Map<Option<String>, u64>,
}

static FULL: LazyLock<parking_lot::Mutex<Full>> = LazyLock::new(Default::default);

/// The directory kept results are written to; unset for a node in memory.
static DIR: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();

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

/// The limits where the cluster settings do not say otherwise. The node's
/// running searches are the plugin's default; one caller gets half of them,
/// and half of the bytes a node keeps, so a second caller always has room.
const NODE_RUNNING: u64 = 20;
const USER_RUNNING: u64 = 10;
const NODE_RETAINED: &str = "256mb";
const USER_RETAINED: &str = "128mb";

/// How often results whose time ran out are looked for.
const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

fn now_millis() -> u64 {
    crate::store::now_millis().max(0) as u64
}

/// A duration setting of the plugin, read from the cluster settings under
/// either of the names the plugin answers to.
fn plugin_limit(store: &Store, key: &str, default: f64) -> (f64, String) {
    if let Some(text) = plugin_setting(store, key)
        && let Some(ms) = crate::tasks::millis_of(&text)
    {
        return (ms, text);
    }
    let text = match key {
        "max_wait_for_completion_timeout" => "1m",
        _ => "5d",
    };
    (default, text.to_string())
}

/// A setting of the plugin as written, under either of its prefixes.
///
/// The settings the cluster manager published come first: a setting is put
/// on the manager, and the store of any other node never hears of it, so a
/// limit read only from this node's store held on the manager alone.
fn plugin_setting(store: &Store, key: &str) -> Option<String> {
    let text = |v: Value| match v {
        Value::String(s) => s,
        other => other.to_string(),
    };
    ["plugins", "opendistro"].iter().find_map(|prefix| {
        let name = format!("{prefix}.asynchronous_search.{key}");
        let published = crate::cluster::with_state(|s| {
            ["transient", "persistent"]
                .iter()
                .find_map(|scope| s.cluster_settings.get(scope).and_then(|o| o.get(&name)).cloned())
        });
        published.filter(|v| !v.is_null()).or_else(|| store.cluster_setting(&name)).map(text)
    })
}

/// The limits in force, read each time: they are dynamic settings.
struct Limits {
    node_running: u64,
    user_running: u64,
    node_bytes: u64,
    user_bytes: u64,
    persist_failures: bool,
}

impl Limits {
    fn read(store: &Store) -> Limits {
        let count = |key: &str, default: u64| {
            plugin_setting(store, key).and_then(|t| t.trim().parse::<u64>().ok()).unwrap_or(default)
        };
        let bytes = |key: &str, default: &str| {
            let text = plugin_setting(store, key).unwrap_or_else(|| default.to_string());
            crate::ingest::bytes_of_text(&text)
                .or_else(|_| crate::ingest::bytes_of_text(default))
                .unwrap_or(0)
                .max(0) as u64
        };
        Limits {
            node_running: count("node_concurrent_running_searches", NODE_RUNNING),
            user_running: count("user_concurrent_running_searches", USER_RUNNING),
            node_bytes: bytes("node_retained_bytes", NODE_RETAINED),
            user_bytes: bytes("user_retained_bytes", USER_RETAINED),
            persist_failures: plugin_setting(store, "persist_search_failures")
                .map(|v| v == "true")
                .unwrap_or(false),
        }
    }
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

/// The node an id was handed out by, when it is an id of ours.
fn node_of(id: &str) -> Option<String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(id).ok()?;
    let len = *bytes.first()? as usize;
    let node = bytes.get(1..1 + len)?;
    String::from_utf8(node.to_vec()).ok()
}

/// Start what asynchronous search keeps between requests: the results a
/// node kept before it stopped, read back from its data directory, and the
/// schedule that lets results go when their time runs out.
pub fn start_asynchronous_search(store: &Store) {
    let dir = store.data_dir().map(|d| d.join("_state").join("asynchronous_search"));
    let _ = DIR.set(dir.clone());
    if let Some(dir) = &dir {
        let _ = std::fs::create_dir_all(dir);
        load_kept(dir);
    }
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async {
            loop {
                tokio::time::sleep(SWEEP_EVERY).await;
                let _ = tokio::task::spawn_blocking(sweep).await;
            }
        });
    }
}

fn kept_path(dir: &std::path::Path, id: &str) -> std::path::PathBuf {
    let name: String = id.bytes().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{name}.json"))
}

/// Read back the results kept on disk: what is still wanted is counted, what
/// ran out while the node was down, or cannot be read, is let go.
fn load_kept(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let now = now_millis();
    let mut kept = KEPT.lock();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            // a temporary file a write did not finish
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let record = std::fs::read(&path).ok().and_then(|b| {
            let len = b.len() as u64;
            serde_json::from_slice::<Value>(&b).ok().map(|v| (v, len))
        });
        let Some((record, bytes)) = record else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let expiration = record["expiration_time_in_millis"].as_u64().unwrap_or(0);
        let Some(id) = record["id"].as_str().filter(|_| expiration > now) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        kept.insert(
            id.to_string(),
            Kept {
                owner: record["owner"].as_str().map(String::from),
                expiration_millis: expiration,
                bytes,
                held: None,
            },
        );
    }
}

/// Let go of searches whose time has run out, stopping any still running,
/// and of kept results whose time has run out.
fn sweep() {
    let now = now_millis();
    {
        let mut held = SEARCHES.lock();
        held.retain(|_, s| {
            let alive = s.expiration_millis.load(Ordering::Relaxed) > now;
            if !alive {
                crate::tasks::cancel(&s.task);
            }
            alive
        });
    }
    let gone: Vec<String> = {
        let mut kept = KEPT.lock();
        let gone: Vec<String> = kept
            .iter()
            .filter(|(_, k)| k.expiration_millis <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &gone {
            if let Some(k) = kept.remove(id) {
                room_freed(&k.owner);
            }
        }
        gone
    };
    if let Some(Some(dir)) = DIR.get() {
        for id in gone {
            let _ = std::fs::remove_file(kept_path(dir, &id));
        }
    }
}

fn not_found(id: &str) -> Response {
    err(
        StatusCode::NOT_FOUND,
        "resource_not_found_exception",
        format!("Either the resource [{id}] does not exist or you do not have access"),
    )
}

/// The plugin's refusal of a search it has no room for.
fn rejected(reason: String) -> Response {
    COUNTS.rejected.fetch_add(1, Ordering::Relaxed);
    err(StatusCode::TOO_MANY_REQUESTS, "asynchronous_search_rejected_exception", reason)
}

/// Whether the caller now may see what this owner submitted.
fn may_see(owner: &Option<String>) -> bool {
    let caller = crate::store::current_owner();
    !matches!((owner, &caller), (Some(owner), Some(caller)) if owner != caller)
}

/// The search a caller may see under this id, while it is in memory.
fn visible(id: &str) -> Option<Arc<Submitted>> {
    let found = SEARCHES.lock().get(id).cloned()?;
    if found.expiration_millis.load(Ordering::Relaxed) <= now_millis() {
        return None;
    }
    may_see(&found.owner).then_some(found)
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

/// Keep a finished search's result, if it was asked to be kept and there is
/// room for it: written down before the search is let go of in memory, so a
/// read in between finds it one place or the other.
fn keep(s: &Submitted, outcome: &Outcome, limits: &Limits) -> Keeping {
    let (field, body) = match outcome {
        Outcome::Running => return Keeping::NotAsked,
        Outcome::Succeeded(v) => ("response", v),
        Outcome::Failed(v) if limits.persist_failures => ("error", v),
        Outcome::Failed(_) => return Keeping::NotAsked,
    };
    if !s.keep_on_completion || s.deleted.load(Ordering::Relaxed) {
        return Keeping::NotAsked;
    }
    let expiration = s.expiration_millis.load(Ordering::Relaxed);
    // a search that outran its own keep_alive has nothing left to be kept for
    if expiration <= now_millis() {
        return Keeping::NotAsked;
    }
    let record = json!({
        "id": s.id,
        "owner": s.owner,
        "start_time_in_millis": s.start_millis,
        "expiration_time_in_millis": expiration,
        field: body,
    });
    let bytes = serde_json::to_vec(&record).unwrap_or_default();
    let size = bytes.len() as u64;
    let dir = DIR.get().cloned().flatten();
    {
        // the room is taken under the lock, before the write, so two searches
        // finishing at once cannot both fit into the last of it
        let mut kept = KEPT.lock();
        let node: u64 = kept.values().map(|k| k.bytes).sum();
        let user: u64 = kept.values().filter(|k| k.owner == s.owner).map(|k| k.bytes).sum();
        if node + size > limits.node_bytes || user + size > limits.user_bytes {
            let mut full = FULL.lock();
            if node + size > limits.node_bytes {
                full.node = Some(limits.node_bytes);
            } else {
                full.users.insert(s.owner.clone(), limits.user_bytes);
            }
            COUNTS.persist_failed.fetch_add(1, Ordering::Relaxed);
            return Keeping::Failed;
        }
        kept.insert(
            s.id.clone(),
            Kept {
                owner: s.owner.clone(),
                expiration_millis: expiration,
                bytes: size,
                held: dir.is_none().then(|| Arc::new(record)),
            },
        );
    }
    if let Some(dir) = dir
        && let Err(e) = crate::store::write_atomic(&kept_path(&dir, &s.id), &bytes)
    {
        tracing::warn!("asynchronous search [{}] could not be kept: {e}", s.id);
        KEPT.lock().remove(&s.id);
        COUNTS.persist_failed.fetch_add(1, Ordering::Relaxed);
        return Keeping::Failed;
    }
    // deleted while it was being written: the delete wins
    if s.deleted.load(Ordering::Relaxed) {
        forget_kept(&s.id);
        return Keeping::NotAsked;
    }
    COUNTS.persisted.fetch_add(1, Ordering::Relaxed);
    Keeping::Kept
}

/// Let go of a kept result, on disk and in the count.
fn forget_kept(id: &str) -> bool {
    let was = KEPT.lock().remove(id);
    if let Some(k) = &was {
        room_freed(&k.owner);
        if let Some(Some(dir)) = DIR.get() {
            let _ = std::fs::remove_file(kept_path(dir, id));
        }
    }
    was.is_some()
}

/// A result of this owner was let go: there is room again, for them and on
/// the node.
fn room_freed(owner: &Option<String>) {
    let mut full = FULL.lock();
    full.node = None;
    full.users.remove(owner);
}

/// A kept result as the caller may read it.
fn read_kept(id: &str) -> Option<Value> {
    let now = now_millis();
    let (held, owner) = {
        let kept = KEPT.lock();
        let k = kept.get(id).filter(|k| k.expiration_millis > now)?;
        (k.held.clone(), k.owner.clone())
    };
    if !may_see(&owner) {
        return None;
    }
    if let Some(v) = held {
        return Some((*v).clone());
    }
    let dir = DIR.get().cloned().flatten()?;
    match std::fs::read(kept_path(&dir, id)).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
        Some(v) => Some(v),
        None => {
            // gone from under the count: it is not there to be read
            KEPT.lock().remove(id);
            None
        }
    }
}

/// Give a kept result a new expiry, where it is kept.
fn extend_kept(id: &str, record: &mut Value, expiration: u64) {
    record["expiration_time_in_millis"] = json!(expiration);
    let mut kept = KEPT.lock();
    let Some(k) = kept.get_mut(id) else { return };
    k.expiration_millis = expiration;
    if k.held.is_some() {
        k.held = Some(Arc::new(record.clone()));
    } else if let Some(Some(dir)) = DIR.get() {
        let bytes = serde_json::to_vec(record).unwrap_or_default();
        k.bytes = bytes.len() as u64;
        let _ = crate::store::write_atomic(&kept_path(dir, id), &bytes);
    }
}

/// Settle a search that has finished and whose submit has answered: a kept
/// result is read from where it was kept from now on, and any other is let go.
fn settle(s: &Submitted) {
    SEARCHES.lock().remove(&s.id);
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

/// A read or delete of an id another node handed out goes to that node: the
/// search runs there and its result is kept there. Nothing is sent on for an
/// id this node handed out, for a request another node already sent here, or
/// when that node cannot be reached -- then this node answers from what it has.
async fn send_to_owner(id: &str, method: axum::http::Method, uri: &Uri) -> Option<Response> {
    if crate::cluster::forward::answering_forward() {
        return None;
    }
    let owner = crate::cluster::NodeId(node_of(id)?);
    if owner.as_str() == crate::tasks::node_id() {
        return None;
    }
    let req = axum::http::Request::builder()
        .method(method)
        .uri(uri.path_and_query().map(|p| p.as_str()).unwrap_or(uri.path()))
        .body(axum::body::Body::empty())
        .ok()?;
    crate::cluster::forward::send_to(&owner, req).await
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
    let owner = crate::store::current_owner();
    let keep_on_completion = flag(&p, "keep_on_completion");
    let limits = Limits::read(&store);
    // a caller who has filled the room kept for results may not ask for more
    // to be kept; the room left is looked at again when the search finishes
    if keep_on_completion {
        let kept = KEPT.lock();
        let node: u64 = kept.values().map(|k| k.bytes).sum();
        let user: u64 = kept.values().filter(|k| k.owner == owner).map(|k| k.bytes).sum();
        drop(kept);
        let full = FULL.lock();
        let node_full = full.node.is_some_and(|allowed| limits.node_bytes <= allowed);
        let user_full = full.users.get(&owner).is_some_and(|allowed| limits.user_bytes <= *allowed);
        drop(full);
        if node >= limits.node_bytes || node_full {
            return rejected(format!(
                "Trying to keep more asynchronous search results than this node allows. The \
                 results kept take [{node}] bytes of the allowed [{}]. This limit can be set by \
                 changing the [plugins.asynchronous_search.node_retained_bytes] setting.",
                limits.node_bytes
            ));
        }
        if user >= limits.user_bytes || user_full {
            return rejected(format!(
                "Trying to keep more asynchronous search results than one user may. The results \
                 kept for this user take [{user}] bytes of the allowed [{}]. This limit can be \
                 set by changing the [plugins.asynchronous_search.user_retained_bytes] setting.",
                limits.user_bytes
            ));
        }
    }
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
        keep_on_completion,
        owner: owner.clone(),
        shards,
        state: parking_lot::Mutex::new((Outcome::Running, false)),
        running: AtomicBool::new(true),
        kept: parking_lot::Mutex::new(Keeping::NotAsked),
        deleted: AtomicBool::new(false),
        task: task.0.clone(),
        done: tokio::sync::Notify::new(),
    });
    // counted and taken under one lock: two submits at once cannot both take
    // the last place
    {
        let mut held = SEARCHES.lock();
        let running = held.values().filter(|s| s.running.load(Ordering::Relaxed));
        let (node, user) =
            running.fold((0u64, 0u64), |(n, u), s| (n + 1, u + u64::from(s.owner == owner)));
        if node >= limits.node_running {
            drop(held);
            return rejected(format!(
                "Trying to create too many concurrent searches. Allowed maximum is [{}]. This \
                 limit can be set by changing the \
                 [plugins.asynchronous_search.node_concurrent_running_searches] setting.",
                limits.node_running
            ));
        }
        if user >= limits.user_running {
            drop(held);
            return rejected(format!(
                "Trying to create too many concurrent searches for one user. Allowed maximum is \
                 [{}]. This limit can be set by changing the \
                 [plugins.asynchronous_search.user_concurrent_running_searches] setting.",
                limits.user_running
            ));
        }
        held.insert(search.id.clone(), search.clone());
    }
    // a request refused before it was a search is not counted as one
    COUNTS.submitted.fetch_add(1, Ordering::Relaxed);
    COUNTS.initialized.fetch_add(1, Ordering::Relaxed);

    // the search itself is the ordinary one, with the plugin's own parameters
    // taken out, run as the caller on a thread of its own
    let mut params = p.clone();
    for own in ["index", "keep_alive", "keep_on_completion", "wait_for_completion_timeout"] {
        params.remove(own);
    }
    let caller = crate::security::layer::current_caller();
    let running = search.clone();
    let handle = tokio::runtime::Handle::current();
    let settings_store = store.clone();
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
        // kept before it is let go of in memory, and read from where it was
        // kept by everyone who asks after the submit has answered
        let keeping = keep(&running, &outcome, &Limits::read(&settings_store));
        *running.kept.lock() = keeping;
        let mut state = running.state.lock();
        state.0 = outcome;
        running.running.store(false, Ordering::Relaxed);
        if state.1 {
            settle(&running);
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
    let keeping = *search.kept.lock();
    let label = match (&state.0, keeping) {
        (Outcome::Running, _) => "RUNNING",
        (_, Keeping::Kept) => "PERSISTING",
        (_, Keeping::Failed) => "PERSIST_FAILED",
        _ => "CLOSED",
    };
    if !matches!(state.0, Outcome::Running) {
        settle(&search);
    }
    answer_for(&search, label, &state.0, &p)
}

/// `GET _plugins/_asynchronous_search/{id}` -- a search read back.
pub async fn get_async_search(
    Path(id): Path<String>,
    Query(p): Query<Params>,
    uri: Uri,
) -> Response {
    let keep_alive = match millis_param(&p, "keep_alive") {
        Ok(v) => v,
        Err(refusal) => return refusal,
    };
    if let Some(answer) = send_to_owner(&id, axum::http::Method::GET, &uri).await {
        return answer;
    }
    if let Some(search) = visible(&id) {
        // reading a search back may give it longer to live
        if let Some((ms, _)) = keep_alive {
            search.expiration_millis.store(now_millis() + ms as u64, Ordering::Relaxed);
        }
        let state = search.state.lock();
        let label = match &state.0 {
            Outcome::Running => "RUNNING",
            Outcome::Succeeded(_) => "SUCCEEDED",
            Outcome::Failed(_) => "FAILED",
        };
        return answer_for(&search, label, &state.0, &p);
    }
    let Some(mut record) = read_kept(&id) else { return not_found(&id) };
    if let Some((ms, _)) = keep_alive {
        extend_kept(&id, &mut record, now_millis() + ms as u64);
    }
    let mut o = serde_json::Map::new();
    o.insert("id".into(), json!(id));
    o.insert("state".into(), json!("STORE_RESIDENT"));
    for key in ["start_time_in_millis", "expiration_time_in_millis", "response", "error"] {
        if let Some(v) = record.get_mut(key).map(Value::take) {
            o.insert(key.into(), v);
        }
    }
    respond(&p, Value::Object(o))
}

/// `DELETE _plugins/_asynchronous_search/{id}` -- stop a search, or forget one
/// that was kept.
pub async fn delete_async_search(
    Path(id): Path<String>,
    Query(p): Query<Params>,
    uri: Uri,
) -> Response {
    if let Some(answer) = send_to_owner(&id, axum::http::Method::DELETE, &uri).await {
        return answer;
    }
    let search = visible(&id);
    if let Some(search) = &search {
        search.deleted.store(true, Ordering::Relaxed);
        SEARCHES.lock().remove(&id);
        if search.running.load(Ordering::Relaxed) {
            crate::tasks::cancel(&search.task);
            COUNTS.cancelled.fetch_add(1, Ordering::Relaxed);
        }
    }
    let kept = read_kept_owner(&id).is_some_and(|owner| may_see(&owner)) && forget_kept(&id);
    if search.is_none() && !kept {
        return not_found(&id);
    }
    respond(&p, json!({"acknowledged": true}))
}

/// Who a kept result belongs to, if one is kept under this id.
fn read_kept_owner(id: &str) -> Option<Option<String>> {
    KEPT.lock().get(id).map(|k| k.owner.clone())
}

/// `GET _plugins/_asynchronous_search/stats` -- what the node has done with
/// asynchronous searches since it started.
pub async fn async_search_stats(Query(p): Query<Params>) -> Response {
    let running = SEARCHES.lock().values().filter(|s| s.running.load(Ordering::Relaxed)).count();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_names_the_node_that_handed_it_out() {
        let id = new_id(7);
        assert_eq!(node_of(&id).as_deref(), Some(crate::tasks::node_id().as_str()));
        assert_eq!(node_of("not base64 at all!"), None);
    }

    #[test]
    fn a_result_whose_time_ran_out_is_let_go_by_the_schedule() {
        let owner = Some("sweep-test".to_string());
        let id = "sweep-test-id".to_string();
        KEPT.lock().insert(
            id.clone(),
            Kept {
                owner: owner.clone(),
                expiration_millis: now_millis().saturating_sub(1),
                bytes: 10,
                held: Some(Arc::new(json!({}))),
            },
        );
        sweep();
        assert!(!KEPT.lock().contains_key(&id));
    }
}
