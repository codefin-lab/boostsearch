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

use super::{Caller, Verdict, is_cluster_action};

tokio::task_local! {
    /// The caller of the request being handled, visible to everything the
    /// handler runs on this task.
    pub static CALLER: Caller;
}

/// The caller of the request in hand, if a request is in hand.
pub fn current_caller() -> Option<Caller> {
    CALLER.try_with(|c| c.clone()).ok()
}

async fn run_as(caller: Caller, req: Request, next: Next) -> Response {
    let mut req = req;
    req.extensions_mut().insert(caller.clone());
    CALLER.scope(caller, next.run(req)).await
}
use crate::store::Store;

/// The subject DN of the client certificate a connection presented.
#[derive(Clone, Debug)]
pub struct PeerDn(pub String);

/// The `401 Unauthorized` the plugin answers, with the challenge asked for.
pub fn unauthorized_with(challenge: &str) -> Response {
    // the plugin's basic challenge says `Unauthorized`; its bearer and
    // SAML challenges say nothing at all
    let body = if challenge.starts_with("Basic") { "Unauthorized" } else { "" };
    let mut r = (StatusCode::UNAUTHORIZED, body).into_response();
    if let Ok(v) = HeaderValue::from_str(challenge) {
        r.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=UTF-8"));
    r
}

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
pub async fn authenticate(State(store): State<Store>, req: Request, next: Next) -> Response {
    let sec = store.security.clone();
    if !sec.enabled {
        return run_as(Caller::unrestricted(), req, next).await;
    }
    // the SAML token exchange is how a caller gets credentials: it runs
    // for anyone, as the plugin runs it inside its challenge
    if req.uri().path().ends_with("/_plugins/_security/api/authtoken") {
        return run_as(Caller::default(), req, next).await;
    }
    let remote = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip().to_string())
        .unwrap_or_default();
    let peer_dn = req.extensions().get::<PeerDn>().map(|d| d.0.clone());
    let query = req.uri().query().unwrap_or("").to_string();
    let audit = sec.audit.clone();
    // a request carrying the plugin's own internal headers is refused, and
    // that refusal is written down
    if req.headers().keys().any(|k| {
        k.as_str().starts_with("_opendistro_security_") || k.as_str().starts_with("_security_")
    }) {
        let (req, body_text) = buffered(req).await;
        let info = request_info(&req, &query, &remote, &body_text);
        audit.bad_headers(&info);
        return bad_headers_response();
    }
    let caller = {
        let presented = super::authc::Presented {
            headers: req.headers(),
            query: &query,
            remote: remote.clone(),
            peer_dn,
        };
        match sec.caller_for(&presented).await {
            Ok(c) => c,
            Err(refusal) => {
                // the body is read only now, for the record of the failure
                let name = presented_name(req.headers());
                let (req, body_text) = buffered(req).await;
                let info = request_info(&req, &query, &remote, &body_text);
                if let Some(name) = name {
                    audit.failed_login(Some(&name), &info);
                }
                return match refusal {
                    super::authc::Refusal::Challenge(ch) => unauthorized_with(&ch),
                    super::authc::Refusal::Forbidden => {
                        forbidden("Authentication finally failed".into())
                    }
                };
            }
        }
    };
    // the body is copied for the log only when a record would quote it;
    // a bulk of a megabyte is otherwise passed straight through
    let path_now = req.uri().path().to_string();
    let method_now = req.method().clone();
    let admin_action = action_for(&method_now, &path_now)
        .map(|a| {
            a.starts_with("indices:admin/")
                && !a.starts_with("indices:admin/get")
                && !a.starts_with("indices:admin/mappings/get")
                && !a.starts_with("indices:admin/aliases/get")
        })
        .unwrap_or(false);
    let (req, body_text) =
        if audit.quotes_bodies(admin_action, path_now.starts_with("/_plugins/_security/api/")) {
            buffered(req).await
        } else {
            (req, String::new())
        };
    let info = request_info(&req, &query, &remote, &body_text);
    audit.authenticated(&caller, &info);
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    // the security API and account endpoints decide for themselves
    if path.starts_with("/_plugins/_security/") {
        if path.starts_with("/_plugins/_security/api/") && sec.may_administer(&caller) {
            audit.granted_rest(&caller, &info);
        }
        return run_as(caller, req, next).await;
    }
    let Some(action) = action_for(&method, &path) else {
        return run_as(caller, req, next).await;
    };
    let named = indices_of(&path);
    let mut resolved: Vec<String> = Vec::new();
    // the guard must be gone before the handler is awaited
    let refusal = {
        let cfg = sec.config.read();
        if is_cluster_action(&action) || named.is_empty() && !action.starts_with("indices:") {
            // the plugin resolves a cluster request to every index it touches
            resolved = store.resolve("*");
            resolved.sort();
            if cfg.cluster_allowed(&caller, &action) { None } else { Some(action.clone()) }
        } else {
            // an index action naming no index is over every index there is
            let indices = if named.is_empty() || named.iter().any(|n| n == "_all") {
                let mut all = store.resolve("*");
                all.sort();
                all
            } else {
                resolve_indices(&store, &named)
            };
            resolved = indices.clone();
            match cfg.index_verdict(&caller, &action, &indices) {
                Verdict::Allowed | Verdict::Partial(_) => None,
                Verdict::Denied { missing } => Some(missing),
            }
        }
    };
    if let Some(missing) = refusal {
        audit.missing_privileges(&caller, &missing, &info, &named, &resolved);
        return no_permissions(&missing, &caller);
    }
    let admin_action = action.starts_with("indices:admin/")
        && !action.starts_with("indices:admin/get")
        && !action.starts_with("indices:admin/mappings/get")
        && !action.starts_with("indices:admin/aliases/get");
    // an index-administration action is written down twice, as the
    // plugin writes it: the grant, and the index event
    audit.granted_privileges(&caller, &action, &info, &named, &resolved);
    // a single document write is a bulk of one inside OpenSearch, and the
    // bulk is granted in its own record
    if matches!(
        action.as_str(),
        "indices:data/write/index" | "indices:data/write/delete" | "indices:data/write/update"
    ) {
        let mut bulk_info = info.clone();
        bulk_info.params.remove("id");
        audit.granted_privileges(&caller, "indices:data/write/bulk", &bulk_info, &[], &[]);
    }
    if admin_action {
        audit.index_event(&caller, &action, &info, &named, &resolved, Some(&body_text));
    }
    run_as(caller, req, next).await
}

/// The request with its body read into memory, and that body as text.
async fn buffered(req: Request) -> (Request, String) {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, crate::api::max_content_bytes() as usize)
        .await
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes).to_string();
    (Request::from_parts(parts, axum::body::Body::from(bytes)), text)
}

/// What the audit log quotes of a request.
fn request_info(req: &Request, query: &str, remote: &str, body: &str) -> super::audit::RequestInfo {
    let path = req.uri().path().to_string();
    let mut params = std::collections::BTreeMap::new();
    // the route's own names, as OpenSearch's REST handlers name them
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    if let Some(first) = segs.first().filter(|s| !s.is_empty() && !s.starts_with('_')) {
        params.insert("index".to_string(), first.to_string());
        if segs.len() >= 3
            && matches!(
                segs[1],
                "_doc" | "_create" | "_update" | "_source" | "_explain" | "_termvectors"
            )
        {
            params.insert("id".to_string(), segs[2].to_string());
        }
    }
    if path.starts_with("/_plugins/_security/api/") && segs.len() >= 5 {
        params.insert("name".to_string(), segs[4].to_string());
    }
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(
            k.to_string(),
            percent_encoding::percent_decode_str(v).decode_utf8_lossy().replace('+', " "),
        );
    }
    let headers = req
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    super::audit::RequestInfo {
        method: req.method().as_str().to_string(),
        path,
        params,
        headers,
        body: if body.is_empty() { None } else { Some(body.to_string()) },
        remote: remote.to_string(),
    }
}

/// The user name a refused request presented, for the failed-login record.
fn presented_name(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(h) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(b) = h.strip_prefix("Basic ").or_else(|| h.strip_prefix("basic ")) {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD.decode(b.trim()).ok()?;
            let text = String::from_utf8_lossy(&bytes).to_string();
            return Some(text.split_once(':').map(|(n, _)| n.to_string()).unwrap_or(text));
        }
        let token = h.trim_start_matches("Bearer ").trim_start_matches("bearer ");
        return jwt_subject(token);
    }
    headers.get("x-proxy-user").and_then(|v| v.to_str().ok()).map(|s| s.to_string())
}

fn jwt_subject(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let _ = parts.next()?;
    let payload = parts.next()?;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("sub").and_then(|s| s.as_str()).map(|s| s.to_string())
}

/// The plugin's refusal of a request carrying its internal headers.
fn bad_headers_response() -> Response {
    let reason = "Illegal parameter in http or transport request found.\nThis means that one node is trying to connect to another with \na non-node certificate (no OID or security.nodes_dn incorrect configured) or that someone \nis spoofing requests. Check your TLS certificate setup as described here: See https://opendistro.github.io/for-elasticsearch-docs/docs/troubleshoot/tls/";
    (StatusCode::FORBIDDEN, axum::Json(json!({"error": {"status": "error", "reason": reason}})))
        .into_response()
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
pub(crate) fn resolve_indices(store: &Store, exprs: &[String]) -> Vec<String> {
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

/// The concrete indices an expression names for a judgement: `_all`, `*`
/// or nothing at all is every index there is.
pub fn indices_for_expr(store: &Store, expr: &str) -> Vec<String> {
    let named: Vec<String> =
        expr.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if named.is_empty() || named.iter().any(|n| n == "_all" || n == "*") {
        let mut all = store.resolve("*");
        all.sort();
        return all;
    }
    resolve_indices(store, &named)
}
