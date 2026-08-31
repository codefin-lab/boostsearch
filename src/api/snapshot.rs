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
    // a repository that already holds snapshots says so as soon as it is
    // registered: the records are on its disk, not in a cluster state this
    // server keeps across a restart
    if let Some(dir) = crate::snapshot::location(&body) {
        let _ = std::fs::create_dir_all(&dir);
        for (snap, record) in crate::snapshot::read_records(&dir) {
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

pub async fn create_snapshot(
    State(store): State<Store>,
    Path((repo, name)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    if !store.repositories().contains_key(&repo) {
        return err(
            StatusCode::NOT_FOUND,
            "repository_missing_exception",
            format!("[{repo}] missing"),
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
    match store.repositories().get(&repo).and_then(crate::snapshot::location) {
        Some(dir) => {
            let kept: Vec<String> = record["indices"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if let Err(e) = crate::snapshot::write(&store, &dir, &name, &kept, &record) {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "repository_exception",
                    format!("[{repo}] could not write snapshot [{name}]: {e}"),
                );
            }
        }
        None => tracing::warn!(
            "snapshot [{name}] in repository [{repo}] records metadata only -- the repository \
             has no filesystem location to copy documents to"
        ),
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
    if !store.repositories().contains_key(&repo) {
        return err(
            StatusCode::NOT_FOUND,
            "repository_missing_exception",
            format!("[{repo}] missing"),
        );
    }
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
    if let Some(dir) = store.repositories().get(&repo).and_then(crate::snapshot::location) {
        for snap in held {
            crate::snapshot::remove(&dir, &snap);
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
    let held = store.snapshots(&repo);
    let Some(source) = held.get(&name) else {
        return err(
            StatusCode::NOT_FOUND,
            "snapshot_missing_exception",
            format!("[{repo}:{name}] is missing"),
        );
    };
    let body: Value = parse_body(&body).unwrap_or_else(|_| json!({}));
    let indices = match body.get("indices") {
        Some(Value::String(s)) => store.resolve(s),
        Some(Value::Array(a)) => {
            let expr: Vec<String> =
                a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
            store.resolve(&expr.join(","))
        }
        _ => source["indices"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default(),
    };
    let global = source["include_global_state"].as_bool().unwrap_or(true);
    let record = snapshot_record(&store, &target, indices, global);
    store.put_snapshot(&repo, &target, record);
    respond(&p, json!({"acknowledged": true}))
}

pub async fn restore_snapshot(
    State(store): State<Store>,
    Path((repo, name)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let held = store.snapshots(&repo);
    let Some(source) = held.get(&name) else {
        return err(
            StatusCode::NOT_FOUND,
            "snapshot_missing_exception",
            format!("[{repo}:{name}] is missing"),
        );
    };
    let body: Value = parse_body(&body).unwrap_or_else(|_| json!({}));
    let held_indices: Vec<String> = source["indices"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
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
    let dir = store.repositories().get(&repo).and_then(crate::snapshot::location);
    // an index comes back from a snapshot open, and says so when asked how it
    // was recovered
    let mut restored = Vec::new();
    for n in held_indices
        .iter()
        .filter(|n| wanted.iter().any(|w| w == *n || crate::store::glob_match(w, n)))
    {
        let target = rename(n);
        if let Some(st) = store.get(&target) {
            // still here: a restore over it says it is back, as it does today
            let mut g = st.write();
            g.closed = false;
            g.restored = true;
            g.save_meta();
            restored.push(target);
            continue;
        }
        // gone: this is what a snapshot is for
        let Some(dir) = dir.as_ref() else {
            continue;
        };
        match crate::snapshot::restore_index(&store, dir, &name, n, &target) {
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
