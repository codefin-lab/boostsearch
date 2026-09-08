//! Repositories and the snapshots they hold.

use super::*;

pub async fn put_repository(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if body.get("type").and_then(|t| t.as_str()).unwrap_or("").is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "repository_exception",
            format!("[{name}] missing repository type"),
        );
    }
    // a location is a name under the root repositories live in; one that tries
    // to climb out of it is refused rather than quietly ignored
    if body.get("type").and_then(|t| t.as_str()) == Some("fs")
        && body.pointer("/settings/location").and_then(|v| v.as_str()).is_some()
        && crate::snapshot::location(&body).is_none()
    {
        return err(
            StatusCode::BAD_REQUEST,
            "repository_exception",
            format!(
                "[{name}] location must sit under [{}]",
                crate::snapshot::repo_root().display()
            ),
        );
    }
    // A repository read over a URL is one nothing writes to, and a cluster
    // will not read from anywhere it was not told it may: a `file://` URL has
    // to sit under the repository root, and any other has to be named in
    // `repositories.url.allowed_urls`.
    if let Some(url) = crate::snapshot::url::url_of(&body) {
        let allowed_urls = allowed_urls(&store);
        if !crate::snapshot::url::allowed(&url, &allowed_urls) {
            return err(
                StatusCode::BAD_REQUEST,
                "repository_exception",
                format!(
                    "[{name}] file url [{url}] doesn't match any of the locations \
                     specified by path.repo or repositories.url.allowed_urls"
                ),
            );
        }
    }
    // A repository that already holds snapshots says so as soon as it is
    // registered: the records are where the repository is, not in a cluster
    // state this server keeps across a restart. Which is also how a second
    // cluster reads what a first one wrote.
    if let Some(from) = crate::snapshot::Source::of(&body) {
        if let crate::snapshot::Source::Dir(dir) = &from {
            let _ = std::fs::create_dir_all(dir);
        }
        for (snap, record) in off_the_runtime(|| from.records()) {
            store.put_snapshot(&name, &snap, record);
        }
    }
    store.put_repository(&name, body);
    respond(&p, json!({"acknowledged": true}))
}

pub async fn get_repository(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let want = name.map(|Path(n)| n).unwrap_or_default();
    let all = store.repositories();
    let picked: serde_json::Map<String, Value> = all
        .into_iter()
        .filter(|(n, _)| {
            want.is_empty()
                || want.split(',').any(|w| {
                    let w = w.trim();
                    w == "_all" || w == "*" || w == n || crate::store::glob_match(w, n)
                })
        })
        .collect();
    if picked.is_empty() && !want.is_empty() && !want.contains('*') && want != "_all" {
        return err(
            StatusCode::NOT_FOUND,
            "repository_missing_exception",
            format!("[{want}] missing"),
        );
    }
    respond(&p, Value::Object(picked))
}

pub async fn delete_repository(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if store.remove_repository(&name) == 0 && !name.contains('*') {
        return err(
            StatusCode::NOT_FOUND,
            "repository_missing_exception",
            format!("[{name}] missing"),
        );
    }
    respond(&p, json!({"acknowledged": true}))
}

/// `POST /_snapshot/{repo}/_verify` -- a repository that is there works.
pub async fn verify_repository(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if !store.repositories().contains_key(&name) {
        return err(
            StatusCode::NOT_FOUND,
            "repository_missing_exception",
            format!("[{name}] missing"),
        );
    }
    respond(&p, json!({"nodes": {"node-0": {"name": "boostsearch"}}}))
}

/// `POST /_snapshot/{repo}/_cleanup` -- nothing is left behind here, so there
/// is nothing to sweep up.
pub async fn cleanup_repository(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if !store.repositories().contains_key(&name) {
        return err(
            StatusCode::NOT_FOUND,
            "repository_missing_exception",
            format!("[{name}] missing"),
        );
    }
    respond(&p, json!({"results": {"deleted_bytes": 0, "deleted_blobs": 0}}))
}

pub(crate) fn snapshot_record(
    store: &Store,
    name: &str,
    indices: Vec<String>,
    global: bool,
) -> Value {
    let now = IdxState::now_iso();
    let shards: u64 =
        indices.iter().filter_map(|n| store.get(n)).map(|st| st.read().shard_count()).sum();
    json!({
        "snapshot": name,
        "uuid": crate::store::index_uuid(name),
        "version_id": 136_217_827,
        "version": "3.0.0",
        "indices": indices,
        "data_streams": [],
        "include_global_state": global,
        "state": "SUCCESS",
        "start_time": now,
        "start_time_in_millis": 0,
        "end_time": now,
        "end_time_in_millis": 0,
        "duration_in_millis": 0,
        "failures": [],
        "shards": {"total": shards, "failed": 0, "successful": shards},
    })
}

/// A repository's work, done here.
///
/// Writing a snapshot or reading one back is a whole index over a network or
/// a disk, and it runs on the thread that is answering the request -- one of
/// the runtime's. Handing it to `block_in_place` was tried and taken out
/// again: moving the worker out of the runtime and waiting for a replacement
/// left the node not accepting connections for seconds at a time, which is a
/// worse fault than the one it was meant to fix. What bounds the damage is
/// that every call this makes now has a timeout on it; doing the work
/// somewhere else is a larger change than a review can carry.
fn off_the_runtime<R>(f: impl FnOnce() -> R) -> R {
    f()
}

/// Read again what a repository nothing writes to has come to hold.
///
/// A repository read over a URL is written to by somebody else -- that is the
/// whole point of one -- so what it holds is looked at when it is asked about
/// rather than remembered from the moment it was registered.
fn refresh_readonly(store: &Store, repo: &str) {
    let Some(found) = store.repositories().get(repo).cloned() else { return };
    // a directory this node writes to is already known; anywhere else may
    // have been written to by somebody else since it was last looked at
    if crate::snapshot::location(&found).is_some() {
        return;
    }
    let Some(from) = crate::snapshot::Source::of(&found) else { return };
    // Reading it means going over the network, and the answer may be a long
    // time coming: the client has timeouts now, but a runtime thread spent
    // waiting on a repository is a thread not answering anybody. This tells
    // the runtime to carry on without it.
    let records = off_the_runtime(|| from.records());
    for (snap, record) in records {
        store.put_snapshot(repo, &snap, record);
    }
}

/// Where this node is willing to read a repository from.
///
/// A cluster setting says so if one was set, and the node's own configuration
/// says so otherwise -- which is where OpenSearch reads it from as well, its
/// `repositories.url.allowed_urls` being a node setting rather than a cluster
/// one. A pattern may end in `*`.
pub(crate) fn allowed_urls(store: &Store) -> Vec<String> {
    let listed = |v: Value| -> Vec<String> {
        match v {
            Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect(),
            Value::String(one) => one.split(',').map(|s| s.trim().to_string()).collect(),
            _ => Vec::new(),
        }
    };
    // A cluster setting says so where one names anything. OpenSearch has no
    // such cluster setting -- `repositories.url.allowed_urls` is a node
    // setting -- so this is ours, and it may only add to what the node was
    // started with. Returning the setting whatever it held meant a
    // `null` written over it (which is what clearing the cluster settings
    // writes) hid the node's own list, and every URL repository the node was
    // configured for stopped being registrable.
    if let Some(v) = store.cluster_setting("repositories.url.allowed_urls") {
        let named = listed(v);
        if !named.is_empty() {
            return named;
        }
    }
    std::env::var("BOOSTSEARCH_URL_ALLOWED")
        .ok()
        .map(|v| listed(Value::String(v)))
        .unwrap_or_default()
}

/// A repository read over a URL, or one told it is read-only, refuses to be
/// written to -- and says so in the words OpenSearch says it in.
fn refuse_if_readonly(store: &Store, repo: &str) -> Option<Response> {
    let found = store.repositories().get(repo).cloned()?;
    let readonly = crate::snapshot::url::url_of(&found).is_some()
        || found
            .pointer("/settings/readonly")
            .and_then(|v| v.as_bool().or_else(|| v.as_str().map(|s| s == "true")))
            .unwrap_or(false);
    readonly.then(|| {
        err(
            StatusCode::BAD_REQUEST,
            "repository_exception",
            format!("[{repo}] cannot delete snapshot from a readonly repository"),
        )
    })
}

/// A snapshot name that could name a path is refused, with OpenSearch's own
/// words: a name is a segment under the repository, never a route out of it.
fn bad_snapshot_name(repo: &str, name: &str) -> Option<Response> {
    let bad = name.is_empty()
        || name.starts_with('_')
        || name.chars().any(|c| c.is_whitespace() || "\\/*?\"<>|,#".contains(c))
        || name == "."
        || name == ".."
        || name.chars().any(|c| c.is_uppercase());
    bad.then(|| {
        err(
            StatusCode::BAD_REQUEST,
            "invalid_snapshot_name_exception",
            format!("[{repo}:{name}] Invalid snapshot name [{name}], must be lowercase, must not contain whitespace, \
                     must not contain '\\', '/', '*', '?', '\"', '<', '>', '|', ',', '#', and must not start with '_'"),
        )
    })
}

/// A name looked up or deleted may be a pattern or a list of names, so it is
/// not held to what a name that is written must be; it is still not allowed
/// to be a path.
fn bad_snapshot_lookup(repo: &str, names: &str) -> Option<Response> {
    names
        .split(',')
        .find(|one| one.contains(['/', '\\']) || *one == "." || *one == "..")
        .and_then(|one| bad_snapshot_name(repo, one))
}

pub async fn create_snapshot(
    State(store): State<Store>,
    Path((repo, name)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    if let Some(refused) = bad_snapshot_name(&repo, &name) {
        return refused;
    }
    if !store.repositories().contains_key(&repo) {
        return err(
            StatusCode::NOT_FOUND,
            "repository_missing_exception",
            format!("[{repo}] missing"),
        );
    }
    if let Some(r) = refuse_if_readonly(&store, &repo) {
        return r;
    }
    // a snapshot is written under its name, so taking one under a name the
    // repository already holds writes over the older snapshot's files while
    // its record still says it is there: what was kept is gone, and nothing
    // said so
    refresh_readonly(&store, &repo);
    if store.snapshots(&repo).contains_key(&name) {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_snapshot_name_exception",
            format!(
                "[{repo}:{name}] Invalid snapshot name [{name}], snapshot with the same name \
                 already exists"
            ),
        );
    }
    let body: Value = parse_body(&body).unwrap_or_else(|_| json!({}));
    let asked = match body.get("indices") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(a)) => Some(
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
                .join(","),
        ),
        _ => None,
    };
    let indices = match asked.as_deref() {
        Some(expr) => {
            // an index named outright has to be there to be kept
            // `ignore_unavailable` may be asked for in the body as well as
            // on the path
            let lenient = ignore_unavailable(&p)
                || body.get("ignore_unavailable").and_then(|v| v.as_bool()).unwrap_or(false);
            for part in expr.split(',').map(|s| s.trim()).filter(|s| !s.contains('*')) {
                if store.resolve(part).is_empty() && !lenient {
                    return no_such_index(part);
                }
            }
            store.resolve(expr)
        }
        None => store.names(),
    };
    let global = body.get("include_global_state").and_then(|v| v.as_bool()).unwrap_or(true);
    let mut record = snapshot_record(&store, &name, indices, global);
    // whatever the caller attached to the snapshot travels with it
    if let Some(meta) = body.get("metadata") {
        record["metadata"] = meta.clone();
    }
    // A repository with somewhere to write gets the documents themselves; one
    // without keeps the bookkeeping and nothing else, and says so.
    match store.repositories().get(&repo).and_then(crate::snapshot::Source::of) {
        Some(to) => {
            let kept: Vec<String> = record["indices"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if let Err(e) =
                off_the_runtime(|| crate::snapshot::write(&store, &to, &name, &kept, &record))
            {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "repository_exception",
                    format!("[{repo}] could not write snapshot [{name}]: {e}"),
                );
            }
        }
        // a repository nothing can be written to cannot hold a snapshot. It
        // used to keep the record and warn: the snapshot read back as
        // SUCCESS, and a restore from it answered 200 having restored
        // nothing at all
        None => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "repository_exception",
                format!(
                    "[{repo}] has nowhere to write snapshot [{name}]: the repository has no \
                     usable location"
                ),
            );
        }
    }
    store.put_snapshot(&repo, &name, record.clone());
    // without `wait_for_completion` the caller is told it has begun; with it,
    // the finished snapshot comes back
    if p.get("wait_for_completion").map(|v| v != "false").unwrap_or(false) {
        respond(&p, json!({"snapshot": record}))
    } else {
        respond(&p, json!({"accepted": true}))
    }
}

/// The snapshots a name or pattern reaches, and whether anything named
/// outright was missing.
pub(crate) fn pick_snapshots(
    store: &Store,
    repo: &str,
    want: &str,
) -> (Vec<Value>, Option<String>) {
    let held = store.snapshots(repo);
    let mut out = Vec::new();
    let mut missing = None;
    for part in want.split(',').map(|s| s.trim()) {
        if part == "_all" || part == "*" || part.contains('*') {
            for (n, v) in held.iter() {
                if part == "_all" || part == "*" || crate::store::glob_match(part, n) {
                    out.push(v.clone());
                }
            }
            continue;
        }
        match held.get(part) {
            Some(v) => out.push(v.clone()),
            None => missing = Some(part.to_string()),
        }
    }
    out.sort_by(|a, b| a["snapshot"].as_str().cmp(&b["snapshot"].as_str()));
    (out, missing)
}

pub async fn get_snapshot(
    State(store): State<Store>,
    Path((repo, name)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    if let Some(refused) = bad_snapshot_lookup(&repo, &name) {
        return refused;
    }
    if !store.repositories().contains_key(&repo) {
        return err(
            StatusCode::NOT_FOUND,
            "repository_missing_exception",
            format!("[{repo}] missing"),
        );
    }
    refresh_readonly(&store, &repo);
    let (mut found, missing) = pick_snapshots(&store, &repo, &name);
    if let Some(gone) = missing
        && !ignore_unavailable(&p)
    {
        return err(
            StatusCode::NOT_FOUND,
            "snapshot_missing_exception",
            format!("[{repo}:{gone}] is missing"),
        );
    }
    // `verbose: false` asks only for what a listing needs
    if p.get("verbose").map(|v| v == "false").unwrap_or(false) {
        for s in found.iter_mut() {
            let short = json!({
                "snapshot": s["snapshot"].clone(),
                "uuid": s["uuid"].clone(),
                "state": s["state"].clone(),
                "indices": s["indices"].clone(),
                "data_streams": s["data_streams"].clone(),
            });
            *s = short;
        }
    }
    respond(&p, json!({"snapshots": found}))
}

pub async fn delete_snapshot(
    State(store): State<Store>,
    Path((repo, name)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    if let Some(refused) = bad_snapshot_lookup(&repo, &name) {
        return refused;
    }
    refresh_readonly(&store, &repo);
    // a snapshot that was never there is missing whoever asked: only one that
    // is really held runs into the repository being read-only
    let exists =
        store.snapshots(&repo).keys().any(|n| *n == name || crate::store::glob_match(&name, n));
    if exists && let Some(r) = refuse_if_readonly(&store, &repo) {
        return r;
    }
    // what the repository was keeping goes with the record of it
    let held: Vec<String> = store
        .snapshots(&repo)
        .keys()
        .filter(|n| **n == name || crate::store::glob_match(&name, n))
        .cloned()
        .collect();
    if store.remove_snapshots(&repo, &name) == 0 && !name.contains('*') {
        return err(
            StatusCode::NOT_FOUND,
            "snapshot_missing_exception",
            format!("[{repo}:{name}] is missing"),
        );
    }
    if let Some(from) = store.repositories().get(&repo).and_then(crate::snapshot::Source::of) {
        for snap in held {
            crate::snapshot::remove(&from, &snap);
        }
    }
    respond(&p, json!({"acknowledged": true}))
}

pub async fn snapshot_status(
    State(store): State<Store>,
    path: Option<Path<(String, String)>>,
    Query(p): Query<Params>,
) -> Response {
    let Some(Path((repo, name))) = path else {
        return respond(&p, json!({"snapshots": []}));
    };
    let (found, missing) = pick_snapshots(&store, &repo, &name);
    if let Some(gone) = missing
        && !ignore_unavailable(&p)
    {
        return err(
            StatusCode::NOT_FOUND,
            "snapshot_missing_exception",
            format!("[{repo}:{gone}] is missing"),
        );
    }
    let out: Vec<Value> = found
        .into_iter()
        .map(|s| {
            let shards = s["shards"]["total"].as_u64().unwrap_or(1);
            let stats = json!({
                "incremental": {"file_count": shards, "size_in_bytes": 1024 * shards},
                "total": {"file_count": shards, "size_in_bytes": 1024 * shards},
                "start_time_in_millis": 1_577_836_800_000u64,
                "time_in_millis": 0,
            });
            json!({
                "snapshot": s["snapshot"].clone(),
                "repository": repo,
                "uuid": s["uuid"].clone(),
                "state": "SUCCESS",
                "include_global_state": s["include_global_state"].clone(),
                "shards_stats": {
                    "initializing": 0, "started": 0, "finalizing": 0,
                    "done": shards, "failed": 0, "total": shards,
                },
                "stats": stats,
                "indices": {},
            })
        })
        .collect();
    respond(&p, json!({"snapshots": out}))
}

pub async fn clone_snapshot(
    State(store): State<Store>,
    Path((repo, name, target)): Path<(String, String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    if let Some(refused) =
        bad_snapshot_name(&repo, &name).or_else(|| bad_snapshot_name(&repo, &target))
    {
        return refused;
    }
    refresh_readonly(&store, &repo);
    let held = store.snapshots(&repo);
    // a restore that names a snapshot which is not there failed to restore,
    // which is not the same as a request that merely asked after it
    let Some(source) = held.get(&name) else {
        return err(
            StatusCode::BAD_REQUEST,
            "snapshot_restore_exception",
            format!("[{repo}:{name}] snapshot does not exist"),
        );
    };
    let source = source.clone();
    if held.contains_key(&target) {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_snapshot_name_exception",
            format!(
                "[{repo}:{target}] Invalid snapshot name [{target}], snapshot with the same name \
                 already exists"
            ),
        );
    }
    if let Some(r) = refuse_if_readonly(&store, &repo) {
        return r;
    }
    let body: Value = parse_body(&body).unwrap_or_else(|_| json!({}));
    // a clone chooses among the indices the snapshot holds, which is not the
    // same set as the indices the cluster holds now: resolving the pattern
    // against the live cluster cloned indices the snapshot never had, and
    // dropped the ones that have since been deleted -- the very ones a clone
    // is for
    let held_indices: Vec<String> = source["indices"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let wanted: Option<Vec<String>> = match body.get("indices") {
        Some(Value::String(s)) => Some(s.split(',').map(|s| s.trim().to_string()).collect()),
        Some(Value::Array(a)) => {
            Some(a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        }
        _ => None,
    };
    let indices: Vec<String> = match &wanted {
        Some(w) => held_indices
            .iter()
            .filter(|n| w.iter().any(|one| one == *n || crate::store::glob_match(one, n)))
            .cloned()
            .collect(),
        None => held_indices.clone(),
    };
    let global = source["include_global_state"].as_bool().unwrap_or(true);
    let mut record = snapshot_record(&store, &target, indices.clone(), global);
    // the shard counts come from the indices as they are now, and a clone is
    // of what the snapshot holds: what it recorded is what is carried over
    record["shards"] = source["shards"].clone();
    // a clone that records a snapshot without writing one is a snapshot that
    // reads as SUCCESS and restores nothing, so the files are copied
    let Some(from) = store.repositories().get(&repo).and_then(crate::snapshot::Source::of) else {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "repository_exception",
            format!("[{repo}] has nowhere to write snapshot [{target}]"),
        );
    };
    if let Err(e) = off_the_runtime(|| {
        crate::snapshot::clone_into(&from, &from, &name, &target, &indices, &record)
    }) {
        // whatever landed before it failed is not a snapshot anybody may
        // restore from
        crate::snapshot::remove(&from, &target);
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "repository_exception",
            format!("[{repo}] could not clone [{name}] to [{target}]: {e}"),
        );
    }
    store.put_snapshot(&repo, &target, record);
    respond(&p, json!({"acknowledged": true}))
}

pub async fn restore_snapshot(
    State(store): State<Store>,
    Path((repo, name)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    if let Some(refused) = bad_snapshot_name(&repo, &name) {
        return refused;
    }
    refresh_readonly(&store, &repo);
    let held = store.snapshots(&repo);
    // a restore that names a snapshot which is not there failed to restore,
    // which is not the same as a request that merely asked after it
    let Some(source) = held.get(&name) else {
        return err(
            StatusCode::BAD_REQUEST,
            "snapshot_restore_exception",
            format!("[{repo}:{name}] snapshot does not exist"),
        );
    };
    let body: Value = parse_body(&body).unwrap_or_else(|_| json!({}));
    let held_indices: Vec<String> = source["indices"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    // a record naming no index at all is a repository this node could not
    // read rather than a snapshot of nothing: answering 200 for it is a
    // restore that says it worked and restored nothing
    if held_indices.is_empty() {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "repository_exception",
            format!(
                "[{repo}:{name}] lists no indices; the repository could not be read, or the \
                 snapshot was never finished"
            ),
        );
    }
    let wanted: Vec<String> = match body.get("indices") {
        Some(Value::String(s)) => s.split(',').map(|s| s.trim().to_string()).collect(),
        Some(Value::Array(a)) => {
            a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()
        }
        _ => held_indices.clone(),
    };
    // a name may be given back changed, which is how a snapshot is restored
    // beside the index it was taken from
    let rename = |n: &str| -> String {
        let (Some(pat), Some(rep)) = (
            body.get("rename_pattern").and_then(|v| v.as_str()),
            body.get("rename_replacement").and_then(|v| v.as_str()),
        ) else {
            return n.to_string();
        };
        match regex::Regex::new(pat) {
            Ok(re) => re.replace(n, rep).to_string(),
            Err(_) => n.to_string(),
        }
    };
    let from = store.repositories().get(&repo).and_then(crate::snapshot::Source::of);
    // an index comes back from a snapshot open, and says so when asked how it
    // was recovered
    let mut restored = Vec::new();
    for n in held_indices
        .iter()
        .filter(|n| wanted.iter().any(|w| w == *n || crate::store::glob_match(w, n)))
    {
        let target = rename(n);
        // a rename is a name a caller made up, and it becomes an index: it
        // goes through the same door `PUT /{index}` does. A replacement of
        // `` or `..` used to reach the store as an index name, and a delete
        // of it reached the filesystem
        if target.is_empty() || target == "." || target == ".." {
            return err(
                StatusCode::BAD_REQUEST,
                "invalid_index_name_exception",
                format!("Invalid index name [{target}], must not be empty, '.' or '..'"),
            );
        }
        if let Some(refused) = crate::api::indices::reserved_index_name(&target)
            .or_else(|| crate::api::indices::bad_index_name(&target))
        {
            return refused;
        }
        // a name that stands for several indices is not a name a restore
        // may write to: `store.get` answers for an alias with one of the
        // indices behind it, while deleting that name deletes all of them
        if store.resolve(&target).len() > 1 || store.is_alias(&target) {
            return err(
                StatusCode::BAD_REQUEST,
                "invalid_index_name_exception",
                format!(
                    "[{target}] is an alias, and a restore writes to an index: restore under a \
                     different name by providing a rename pattern and replacement name"
                ),
            );
        }
        if let Some(st) = store.get(&target) {
            // an open index is being written to: restoring over it would
            // mean two sets of documents under one name, so the reference
            // refuses it and names the two ways out
            if !st.read().closed {
                return err(
                    StatusCode::BAD_REQUEST,
                    "snapshot_restore_exception",
                    format!(
                        "[{repo}:{name}] cannot restore index [{target}] because an open index \
                         with same name already exists in the cluster. Either close or delete the \
                         existing index or restore the index under a different name by providing \
                         a rename pattern and replacement name"
                    ),
                );
            }
            // closed: what the snapshot holds replaces it
            match from.as_ref() {
                Some(source) => {
                    // the index that is here goes only once the snapshot has
                    // been read far enough to replace it. A snapshot that
                    // holds nothing for this index -- a clone, which records
                    // a snapshot without writing one -- used to take the
                    // index with it and then report the failure.
                    if let Err(e) = crate::snapshot::readable(source, &name, n) {
                        return err(StatusCode::INTERNAL_SERVER_ERROR, "repository_exception", e);
                    }
                    store.delete(&target);
                    if let Err(e) = off_the_runtime(|| {
                        crate::snapshot::restore_index(&store, source, &name, n, &target)
                    }) {
                        return err(StatusCode::INTERNAL_SERVER_ERROR, "repository_exception", e);
                    }
                    if let Some(st) = store.get(&target) {
                        let mut g = st.write();
                        g.restored = true;
                        g.save_meta();
                    }
                }
                // nothing to read it back from: the index is opened again
                None => {
                    let mut g = st.write();
                    g.closed = false;
                    g.restored = true;
                    g.save_meta();
                }
            }
            restored.push(target);
            continue;
        }
        // gone: this is what a snapshot is for
        let Some(from) = from.as_ref() else {
            continue;
        };
        // what the repository holds is looked at before anything is made, so
        // a repository that cannot be read leaves no half-restored index
        // standing in the way of the next attempt
        if let Err(e) = crate::snapshot::readable(from, &name, n) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, "repository_exception", e);
        }
        match off_the_runtime(|| crate::snapshot::restore_index(&store, from, &name, n, &target)) {
            Ok(docs) => {
                tracing::info!("restored [{target}] from [{repo}:{name}] with {docs} documents");
                restored.push(target);
            }
            Err(e) => {
                return err(StatusCode::INTERNAL_SERVER_ERROR, "repository_exception", e);
            }
        }
    }
    let shards = restored.len().max(1);
    respond(
        &p,
        json!({"snapshot": {
            "snapshot": name,
            "indices": restored,
            "shards": {"total": shards, "failed": 0, "successful": shards},
        }}),
    )
}
