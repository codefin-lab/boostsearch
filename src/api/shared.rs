//! The small things every handler needs: errors, parameters, and the shapes
//! of an answer.

use super::*;

/// The trace `error_trace` asks for.
///
/// OpenSearch answers this with a Java stack trace; this engine has its own,
/// and names the error the way the API already names it -- `type` is that
/// same name in snake case, so the two agree about what went wrong and differ
/// only about which language it went wrong in.
pub fn stack_trace_for(kind: &str, reason: &str, at: &str) -> String {
    let class: String = kind
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect();
    // the reason names the resource, and the class name follows it, which is
    // the order the older Java form put them in
    format!("{reason} -- {class} at boostsearch::api::{at} (src/api.rs)")
}

/// Attach a trace to an error a caller asked to see the inside of.
pub fn add_stack_trace(node: &mut Value, p: &Params, at: &str) {
    if !p.get("error_trace").map(|v| v != "false").unwrap_or(false) {
        return;
    }
    let (kind, reason) = match (
        node.pointer("/type").and_then(|v| v.as_str()),
        node.pointer("/reason").and_then(|v| v.as_str()),
    ) {
        (Some(k), Some(r)) => (k.to_string(), r.to_string()),
        _ => return,
    };
    let trace = stack_trace_for(&kind, &reason, at);
    if let Some(o) = node.as_object_mut() {
        o.insert("stack_trace".into(), json!(trace));
        if let Some(causes) = o.get_mut("root_cause").and_then(|c| c.as_array_mut()) {
            for c in causes {
                if let Some(co) = c.as_object_mut() {
                    co.insert("stack_trace".into(), json!(trace));
                }
            }
        }
    }
}

pub fn err(status: StatusCode, kind: &str, reason: impl Into<String>) -> Response {
    let reason = reason.into();
    let mut r = (
        status,
        axum::Json(json!({
            "error": {"type": kind, "reason": reason, "root_cause": [{"type": kind, "reason": reason}]},
            "status": status.as_u16()
        })),
    )
        .into_response();
    // the error travels with the response, so that whoever catches it can
    // read what it was without opening the body
    r.extensions_mut().insert(ErrorKind { kind: kind.to_string(), reason });
    r
}

/// What an error response says, kept beside its body.
#[derive(Clone, Debug)]
pub struct ErrorKind {
    pub kind: String,
    pub reason: String,
}

/// An error that quotes an inner cause, the shape the search API uses.
pub fn err_caused_by(kind: &str, reason: &str, cause: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({
            "error": {
                "type": kind,
                "reason": reason,
                "root_cause": [{"type": kind, "reason": reason}],
                "caused_by": {"type": "illegal_argument_exception", "reason": cause}
            },
            "status": 400
        })),
    )
        .into_response()
}

pub fn no_such_index(name: &str) -> Response {
    let reason = format!("no such index [{name}]");
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({
            "error": {
                "type": "index_not_found_exception",
                "reason": reason,
                "index": name,
                "resource.type": "index_or_alias",
                "resource.id": name,
                "index_uuid": "_na_",
                "root_cause": [{"type": "index_not_found_exception", "reason": reason, "index": name}]
            },
            "status": 404
        })),
    )
        .into_response()
}

pub(crate) fn flag(p: &Params, key: &str) -> bool {
    matches!(p.get(key).map(|s| s.as_str()), Some("true") | Some("") | Some("wait_for"))
}

pub(crate) fn ignore_unavailable(p: &Params) -> bool {
    // `allow_no_indices=false` overrides ignore_unavailable: an expression that
    // resolves to nothing is still an error
    if p.get("allow_no_indices").map(|v| v == "false").unwrap_or(false) {
        return false;
    }
    flag(p, "ignore_unavailable")
}

/// `?ignore=404` suppresses the error the test would otherwise catch.
pub(crate) fn ignored(p: &Params, status: StatusCode) -> bool {
    p.get("ignore")
        .map(|v| v.split(',').any(|c| c.trim() == status.as_u16().to_string()))
        .unwrap_or(false)
}

/// A size as written on a condition: a count and a unit.
pub(crate) fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    let (n, unit) = s.split_at(split);
    let n: f64 = n.parse().ok()?;
    let scale: u64 = match unit.trim() {
        "b" => 1,
        "kb" => 1024,
        "mb" => 1024 * 1024,
        "gb" => 1024 * 1024 * 1024,
        "tb" => 1024u64.pow(4),
        _ => return None,
    };
    Some((n * scale as f64) as u64)
}

/// A keep-alive as written, in milliseconds.
pub(crate) fn keep_alive_millis(s: &str) -> u64 {
    parse_keep_alive(s).map(|secs| secs * 1000).unwrap_or(0)
}

pub fn merge_into(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                match b.get_mut(k) {
                    Some(slot) if slot.is_object() && v.is_object() => merge_into(slot, v),
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, p) => *b = p.clone(),
    }
}

pub(crate) fn as_list(p: &Params, key: &str) -> Option<Vec<String>> {
    p.get(key)
        .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

pub async fn not_ported() -> Response {
    err(StatusCode::NOT_IMPLEMENTED, "not_implemented_exception", "not ported yet")
}

pub fn _unused(_: Result<()>) {}

// -------------------------------------------------------------------- search

pub fn source_selector_from_params_pub(p: &Params) -> Option<Value> {
    source_selector_from_params(p)
}

/// Every JSON response goes through here so `filter_path` works uniformly.
pub fn respond(p: &Params, v: Value) -> Response {
    match p.get("filter_path") {
        Some(spec) if !spec.is_empty() => {
            axum::Json(crate::source::filter_path(&v, spec)).into_response()
        }
        _ => axum::Json(v).into_response(),
    }
}

pub(crate) fn parse_body(body: &str) -> std::result::Result<Value, Response> {
    if body.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(body)
        .map_err(|e| err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string()))
}

/// Query-string forms of the search request that the suite exercises.
pub(crate) fn fold_params_into_body(body: &mut Value, p: &Params) {
    if let Some(q) = p.get("q")
        && body.get("query").is_none()
    {
        let mut qs = json!({"query": q});
        if let Some(df) = p.get("df").or_else(|| p.get("default_field")) {
            qs["default_field"] = json!(df);
        }
        if let Some(op) = p.get("default_operator") {
            qs["default_operator"] = json!(op.to_lowercase());
        }
        // the caller may name the analyzer the query itself is cut with
        for named in ["analyzer", "quote_analyzer", "minimum_should_match"] {
            if let Some(v) = p.get(named) {
                qs[named] = json!(v);
            }
        }
        for named in ["lenient", "analyze_wildcard", "allow_leading_wildcard"] {
            if let Some(v) = p.get(named) {
                qs[named] = json!(v == "true");
            }
        }
        body["query"] = json!({ "query_string": qs });
    }
    for key in ["from", "size", "track_total_hits"] {
        if let (Some(v), None) = (p.get(key), body.get(key)) {
            body[key] = match v.as_str() {
                "true" => json!(true),
                "false" => json!(false),
                s => s.parse::<i64>().map(|n| json!(n)).unwrap_or(json!(s)),
            };
        }
    }
    if let Some(s) = p.get("sort")
        && body.get("sort").is_none()
    {
        let items: Vec<Value> = s
            .split(',')
            .map(|part| match part.split_once(':') {
                Some((f, o)) => json!({ f: o }),
                None => json!(part),
            })
            .collect();
        body["sort"] = Value::Array(items);
    }
}

/// A keep-alive or scroll timeout as written: a count and a unit.
pub(crate) fn parse_keep_alive(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let (n, unit) = s.split_at(split);
    let n: u64 = n.parse().ok()?;
    Some(match unit {
        "ms" => n / 1000,
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => return None,
    })
}

/// OpenSearch refuses a field whose name is nothing but dots.
pub(crate) fn dotted_only_field(v: &Value) -> Option<String> {
    match v {
        Value::Object(o) => {
            for (k, child) in o {
                if !k.is_empty() && k.chars().all(|c| c == '.') {
                    return Some(k.clone());
                }
                if let Some(found) = dotted_only_field(child) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(a) => a.iter().find_map(dotted_only_field),
        _ => None,
    }
}

pub(crate) fn g_has_writer(st: &std::sync::Arc<parking_lot::RwLock<IdxState>>) -> bool {
    st.read().has_writer()
}

/// `_id` and `_index` may arrive as strings or bare numbers.
pub(crate) fn scalar_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A size the way `cat` writes one: a number and the unit it is in.
pub(crate) fn readable_bytes(bytes: u64) -> String {
    const UNITS: [(u64, &str); 4] =
        [(1024 * 1024 * 1024, "gb"), (1024 * 1024, "mb"), (1024, "kb"), (1, "b")];
    for (scale, unit) in UNITS {
        if bytes >= scale {
            if scale == 1 {
                return format!("{bytes}b");
            }
            let n = bytes as f64 / scale as f64;
            return format!("{n:.1}{unit}");
        }
    }
    "0b".to_string()
}

/// Are these two names a single character apart -- one changed, added, or
/// dropped? Close enough to be worth suggesting when a metric is not known.
pub(crate) fn one_edit_apart(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let (long, short) = if a.len() >= b.len() { (&a, &b) } else { (&b, &a) };
    let (mut i, mut j, mut edits) = (0, 0, 0);
    while i < long.len() && j < short.len() {
        if long[i] == short[j] {
            i += 1;
            j += 1;
            continue;
        }
        edits += 1;
        if edits > 1 {
            return false;
        }
        if long.len() == short.len() {
            i += 1;
            j += 1;
        } else {
            i += 1;
        }
    }
    edits + (long.len() - i) <= 1
}

/// Cluster settings are held with dotted keys and string values, which is the
/// flat form. `flat_settings=false`, the default, reports them as a tree.
pub(crate) fn nest_settings(flat: &Value) -> Value {
    let mut out = json!({});
    let Some(o) = flat.as_object() else { return out };
    for (k, v) in o {
        let path: Vec<&str> = k.split('.').collect();
        let mut cur = &mut out;
        for part in &path[..path.len() - 1] {
            cur = entry_of(cur, part, || json!({}));
            if !cur.is_object() {
                *cur = json!({});
            }
        }
        if let Some(m) = cur.as_object_mut() {
            m.insert(path[path.len() - 1].to_string(), v.clone());
        }
    }
    out
}

/// Could one name match both of these patterns?
///
/// Two templates overlap when some index name would pick up both, which is
/// not the same as their patterns being written the same way: `t*` and `te*`
/// both claim `test`.
pub(crate) fn patterns_overlap(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let head = |p: &str| p.split('*').next().unwrap_or(p).to_string();
    let (ha, hb) = (head(a), head(b));
    // with the wildcards taken off, one claim contains the other when its
    // fixed part is a prefix of the other's
    if a.contains('*') && b.contains('*') {
        return ha.starts_with(&hb) || hb.starts_with(&ha);
    }
    if a.contains('*') {
        return crate::store::glob_match(a, b);
    }
    if b.contains('*') {
        return crate::store::glob_match(b, a);
    }
    false
}

pub(crate) fn num_cpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// The value at `key` inside `node`, made by `make` if it is not there.
///
/// A JSON value that should be an object and is not gets replaced by one:
/// these are answers this server is building, not documents a client sent, so
/// there is nothing to preserve and nothing to complain about.
pub(crate) fn entry_of<'a>(
    node: &'a mut Value,
    key: &str,
    make: impl FnOnce() -> Value,
) -> &'a mut Value {
    if !node.is_object() {
        *node = json!({});
    }
    node.as_object_mut()
        .expect("replaced with an object just above")
        .entry(key.to_string())
        .or_insert_with(make)
}

/// Where this node listens, as it reports itself.
pub fn bound_address() -> String {
    std::env::var("BOOSTSEARCH_ADDR").unwrap_or_else(|_| "127.0.0.1:9200".to_string())
}

/// The largest body this node accepts, in bytes.
pub fn max_content_bytes() -> u64 {
    std::env::var("BOOSTSEARCH_MAX_CONTENT_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(100)
        * 1024
        * 1024
}

/// What a failed request answered with, as one entry of a list of answers.
///
/// A multi-search reports each search's own complaint rather than a single
/// verdict over all of them, so the answer a handler already built is read
/// back rather than replaced.
pub(crate) async fn as_error_body(response: Response) -> Value {
    let status = response.status().as_u16();
    let read = axum::body::to_bytes(response.into_body(), usize::MAX).await;
    let parsed: Value = read
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or_else(|| json!({}));
    let mut out = json!({ "status": status });
    if let Some(error) = parsed.get("error") {
        out["error"] = error.clone();
    }
    out
}

/// Every index the cluster holds, whether or not a copy is on this node.
///
/// The store is what this node has; on a cluster an index lives wherever the
/// manager placed it, and a listing or a wildcard that stopped at the local
/// store would show one node's share of the cluster as though it were all of
/// it.
pub fn cluster_names(store: &crate::store::Store) -> Vec<String> {
    let mut names = store.names();
    for n in crate::cluster::current_state().indices.keys() {
        if !names.contains(n) {
            names.push(n.clone());
        }
    }
    names.sort();
    names
}

/// As `Store::resolve`, over every index the cluster holds.
pub fn cluster_resolve(store: &crate::store::Store, expr: &str) -> Vec<String> {
    let mut names = store.resolve(expr);
    let state = crate::cluster::current_state();
    for part in expr.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()) {
        if part.starts_with('-') {
            let pattern = part.trim_start_matches('-');
            names.retain(|n| !matches_pattern(pattern, n));
            continue;
        }
        for (name, meta) in &state.indices {
            if names.contains(name) {
                continue;
            }
            let by_alias = meta
                .aliases
                .as_object()
                .map(|a| a.keys().any(|al| matches_pattern(part, al)))
                .unwrap_or(false);
            if matches_pattern(part, name) || by_alias {
                names.push(name.clone());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// `*` and `?` as an index expression means them, and nothing else does.
fn matches_pattern(pattern: &str, name: &str) -> bool {
    if pattern == "_all" || pattern == "*" {
        return true;
    }
    if !pattern.contains('*') && !pattern.contains('?') {
        return pattern == name;
    }
    let mut re = String::from("^");
    for c in pattern.chars() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    regex::Regex::new(&re).map(|r| r.is_match(name)).unwrap_or(false)
}
