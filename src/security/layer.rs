//! The gate every request passes: who is asking, and whether they may.
//!
//! Authentication reads the `Authorization` header and answers 401 the way
//! the plugin does (`text/plain` `Unauthorized`, with a Basic challenge).
//! Authorization maps the request to the transport action it stands for
//! and asks the evaluator; a refusal is the plugin's `security_exception`.

use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::{AuthFailure, Caller, Verdict, is_cluster_action};
use crate::store::Store;

/// The `401 Unauthorized` the plugin answers.
pub fn unauthorized() -> Response {
    let mut r = (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    r.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"OpenSearch Security\""),
    );
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=UTF-8"));
    r
}

/// The `403` `security_exception` the plugin answers.
pub fn forbidden(reason: String) -> Response {
    let body = json!({
        "error": {
            "root_cause": [{"type": "security_exception", "reason": reason}],
            "type": "security_exception",
            "reason": reason,
        },
        "status": 403,
    });
    (StatusCode::FORBIDDEN, axum::Json(body)).into_response()
}

/// `no permissions for [action] and User [...]`
pub fn no_permissions(action: &str, caller: &Caller) -> Response {
    forbidden(format!("no permissions for [{action}] and {}", caller.describe()))
}

/// Work out the caller and put it on the request; refuse what they may
/// not do before the handler runs.
pub async fn authenticate(State(store): State<Store>, mut req: Request, next: Next) -> Response {
    let sec = store.security.clone();
    if !sec.enabled {
        req.extensions_mut().insert(Caller::unrestricted());
        return next.run(req).await;
    }
    let remote = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip().to_string())
        .unwrap_or_default();
    let header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let caller = match sec.caller_from_basic(header.as_deref(), &remote) {
        Ok(c) => c,
        Err(AuthFailure::Challenge) | Err(AuthFailure::Failed) => return unauthorized(),
    };
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    // the security API and account endpoints decide for themselves
    if path.starts_with("/_plugins/_security/") {
        req.extensions_mut().insert(caller);
        return next.run(req).await;
    }
    let Some(action) = action_for(&method, &path) else {
        req.extensions_mut().insert(caller);
        return next.run(req).await;
    };
    // the guard must be gone before the handler is awaited
    let refusal = {
        let cfg = sec.config.read();
        if is_cluster_action(&action)
            || indices_of(&path).is_empty() && !action.starts_with("indices:")
        {
            if cfg.cluster_allowed(&caller, &action) { None } else { Some(action.clone()) }
        } else {
            // an index action naming no index is over every index there is
            let named = indices_of(&path);
            let indices = if named.is_empty() || named.iter().any(|n| n == "_all") {
                let mut all = store.resolve("*");
                all.sort();
                all
            } else {
                resolve_indices(&store, &named)
            };
            match cfg.index_verdict(&caller, &action, &indices) {
                Verdict::Allowed | Verdict::Partial(_) => None,
                Verdict::Denied { missing } => Some(missing),
            }
        }
    };
    if let Some(missing) = refusal {
        return no_permissions(&missing, &caller);
    }
    req.extensions_mut().insert(caller);
    next.run(req).await
}

/// The index expression a path names, split on commas; nothing for
/// paths that name no index.
pub fn indices_of(path: &str) -> Vec<String> {
    let trimmed = path.trim_start_matches('/');
    let first = trimmed.split('/').next().unwrap_or("");
    if first.is_empty() || (first.starts_with('_') && first != "_all") {
        return Vec::new();
    }
    first.split(',').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect()
}

/// Wildcards and aliases resolved to the concrete indices, so that a
/// pattern is judged by what it reaches; a name that reaches nothing is
/// judged as itself.
fn resolve_indices(store: &Store, exprs: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for e in exprs {
        let stripped = e.trim_start_matches('-').trim_start_matches('+');
        if e.starts_with('-') {
            continue;
        }
        let found = store.resolve(stripped);
        if found.is_empty() {
            out.push(stripped.to_string());
        } else {
            out.extend(found);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The transport action a REST request stands for.
pub fn action_for(method: &Method, path: &str) -> Option<String> {
    let p = path.trim_end_matches('/');
    let segs: Vec<&str> = p.trim_start_matches('/').split('/').collect();
    let first = segs.first().copied().unwrap_or("");
    let has_index = !first.is_empty() && (!first.starts_with('_') || first == "_all");
    let rest: Vec<&str> = if has_index { segs[1..].to_vec() } else { segs.clone() };
    let tail = rest.first().copied().unwrap_or("");
    let m = method.as_str();
    let a = match (has_index, tail, m) {
        (false, "", _) => "cluster:monitor/main",
        (_, "_search", _) => "indices:data/read/search",
        (_, "_msearch", _) => "indices:data/read/msearch",
        (_, "_count", _) => "indices:data/read/search",
        (_, "_explain", _) => "indices:data/read/explain",
        (_, "_search_shards", _) => "indices:admin/shards/search_shards",
        (_, "_field_caps", _) => "indices:data/read/field_caps",
        (_, "_validate", _) => "indices:admin/validate/query",
        (_, "_termvectors", _) => "indices:data/read/tv",
        (_, "_mtermvectors", _) => "indices:data/read/mtv",
        (_, "_mget", _) => "indices:data/read/mget",
        (_, "_bulk", _) => "indices:data/write/bulk",
        (_, "_delete_by_query", _) => "indices:data/write/delete/byquery",
        (_, "_update_by_query", _) => "indices:data/write/update/byquery",
        (false, "_reindex", _) => "indices:data/write/reindex",
        (_, "_doc", "GET" | "HEAD") => "indices:data/read/get",
        (_, "_source", _) => "indices:data/read/get",
        (_, "_doc", "DELETE") => "indices:data/write/delete",
        (_, "_doc", _) => "indices:data/write/index",
        (_, "_create", _) => "indices:data/write/index",
        (_, "_update", _) => "indices:data/write/update",
        (_, "_mapping", "GET" | "HEAD") => "indices:admin/mappings/get",
        (_, "_mappings", "GET" | "HEAD") => "indices:admin/mappings/get",
        (_, "_mapping", _) => "indices:admin/mapping/put",
        (_, "_mappings", _) => "indices:admin/mapping/put",
        (_, "_settings", "GET") => "indices:monitor/settings/get",
        (_, "_settings", _) => "indices:admin/settings/update",
        (_, "_alias" | "_aliases", "GET" | "HEAD") => "indices:admin/aliases/get",
        (_, "_alias" | "_aliases", _) => "indices:admin/aliases",
        (_, "_refresh", _) => "indices:admin/refresh",
        (_, "_flush", _) => "indices:admin/flush",
        (_, "_forcemerge", _) => "indices:admin/forcemerge",
        (_, "_cache", _) => "indices:admin/cache/clear",
        (_, "_open", _) => "indices:admin/open",
        (_, "_close", _) => "indices:admin/close",
        (_, "_stats", _) => "indices:monitor/stats",
        (_, "_segments", _) => "indices:monitor/segments",
        (_, "_recovery", _) => "indices:monitor/recovery",
        (_, "_shard_stores", _) => "indices:monitor/shard_stores",
        (_, "_analyze", _) => "indices:admin/analyze",
        (_, "_rollover", _) => "indices:admin/rollover",
        (_, "_shrink", _) => "indices:admin/resize",
        (_, "_split", _) => "indices:admin/resize",
        (_, "_clone", _) => "indices:admin/resize",
        (_, "_block", _) => "indices:admin/block/add",
        (_, "_rank_eval", _) => "indices:data/read/search",
        (_, "_pit", "DELETE") => "indices:data/read/point_in_time/delete",
        (_, "_pit", _) => "indices:data/read/point_in_time/create",
        (_, "_search_pipeline", _) => "cluster:admin/search/pipeline/get",
        (true, "", "GET" | "HEAD") => "indices:admin/get",
        (true, "", "PUT") => "indices:admin/create",
        (true, "", "DELETE") => "indices:admin/delete",
        (true, _, "GET" | "HEAD") if !tail.starts_with('_') => "indices:data/read/get",
        (true, _, "DELETE") if !tail.starts_with('_') => "indices:data/write/delete",
        (true, _, _) if !tail.starts_with('_') => "indices:data/write/index",
        (false, "_cluster", _) => match rest.get(1).copied().unwrap_or("") {
            "health" => "cluster:monitor/health",
            "state" => "cluster:monitor/state",
            "stats" => "cluster:monitor/stats",
            "settings" if m == "GET" => "cluster:admin/settings/get",
            "settings" => "cluster:admin/settings/update",
            "pending_tasks" => "cluster:monitor/task",
            "allocation" => "cluster:admin/allocation/explain",
            "reroute" => "cluster:admin/reroute",
            "voting_config_exclusions" => "cluster:admin/voting_config/add_exclusions",
            _ => "cluster:monitor/state",
        },
        (false, "_nodes", _) => "cluster:monitor/nodes/info",
        (false, "_cat", _) => match rest.get(1).copied().unwrap_or("") {
            "indices" => "indices:monitor/settings/get",
            "aliases" => "indices:admin/aliases/get",
            "shards" | "segments" | "count" | "recovery" => "indices:monitor/stats",
            _ => "cluster:monitor/state",
        },
        (false, "_tasks", _) => "cluster:monitor/task",
        (false, "_template", "GET" | "HEAD") => "indices:admin/template/get",
        (false, "_template", "DELETE") => "indices:admin/template/delete",
        (false, "_template", _) => "indices:admin/template/put",
        (false, "_index_template", "GET" | "HEAD") => "indices:admin/index_template/get",
        (false, "_index_template", "DELETE") => "indices:admin/index_template/delete",
        (false, "_index_template", _) => "indices:admin/index_template/put",
        (false, "_component_template", "GET" | "HEAD") => "cluster:admin/component_template/get",
        (false, "_component_template", "DELETE") => "cluster:admin/component_template/delete",
        (false, "_component_template", _) => "cluster:admin/component_template/put",
        (false, "_ingest", "GET") => "cluster:admin/ingest/pipeline/get",
        (false, "_ingest", "DELETE") => "cluster:admin/ingest/pipeline/delete",
        (false, "_ingest", _)
            if rest.get(2) == Some(&"_simulate") || rest.get(1) == Some(&"_simulate") =>
        {
            "cluster:admin/ingest/pipeline/simulate"
        }
        (false, "_ingest", _) => "cluster:admin/ingest/pipeline/put",
        (false, "_scripts", "GET") => "cluster:admin/script/get",
        (false, "_scripts", "DELETE") => "cluster:admin/script/delete",
        (false, "_scripts", _) => "cluster:admin/script/put",
        (false, "_data_stream", "GET") => "indices:admin/data_stream/get",
        (false, "_data_stream", "DELETE") => "indices:admin/data_stream/delete",
        (false, "_data_stream", _) => "indices:admin/data_stream/create",
        (false, "_snapshot", "GET") => "cluster:admin/snapshot/get",
        (false, "_snapshot", "DELETE") => "cluster:admin/snapshot/delete",
        (false, "_snapshot", _) => "cluster:admin/snapshot/create",
        (false, "_render", _) => "cluster:admin/script/get",
        (false, "_resolve", _) => "indices:admin/resolve/index",
        _ => return None,
    };
    Some(a.to_string())
}
