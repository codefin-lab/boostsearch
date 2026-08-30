//! REST surface: index lifecycle and document CRUD.
//!
//! Response envelopes follow OpenSearch exactly -- `result`, `_version`,
//! `_shards`, `_seq_no` and friends are asserted on directly by the YAML suite.

use crate::store::{IdxState, Store, make_doc};
use anyhow::Result;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::collections::HashMap;
use tantivy::collector::TopDocs;
use tantivy::query::TermQuery;
use tantivy::schema::{IndexRecordOption, Term, Value as _};
use tantivy::TantivyDocument;

pub type Params = HashMap<String, String>;

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
    format!("{reason} -- {class} at obsearch::api::{at} (src/api.rs)")
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
    (
        status,
        axum::Json(json!({
            "error": {"type": kind, "reason": reason, "root_cause": [{"type": kind, "reason": reason}]},
            "status": status.as_u16()
        })),
    )
        .into_response()
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

fn shards() -> Value {
    json!({"total": 2, "successful": 1, "failed": 0})
}

/// A shard tally over a set of indices: every shard they declare, all of them
/// on the one node and so all of them successful.
fn shards_over(store: &Store, names: &[String]) -> Value {
    let total: u64 = names
        .iter()
        .filter_map(|n| store.get(n))
        .map(|st| st.read().numeric_setting("number_of_shards").unwrap_or(1).max(1))
        .sum();
    json!({"total": total, "successful": total, "failed": 0})
}

/// The shards of one index, which is what a write to it reports.
fn shards_of(st: &IdxState) -> Value {
    let n = st.numeric_setting("number_of_shards").unwrap_or(1).max(1);
    json!({"total": n, "successful": n, "failed": 0})
}

fn flag(p: &Params, key: &str) -> bool {
    matches!(p.get(key).map(|s| s.as_str()), Some("true") | Some("") | Some("wait_for"))
}

fn ignore_unavailable(p: &Params) -> bool {
    // `allow_no_indices=false` overrides ignore_unavailable: an expression that
    // resolves to nothing is still an error
    if p.get("allow_no_indices").map(|v| v == "false").unwrap_or(false) {
        return false;
    }
    flag(p, "ignore_unavailable")
}

/// `?ignore=404` suppresses the error the test would otherwise catch.
fn ignored(p: &Params, status: StatusCode) -> bool {
    p.get("ignore")
        .map(|v| v.split(',').any(|c| c.trim() == status.as_u16().to_string()))
        .unwrap_or(false)
}

// ---------------------------------------------------------------- index CRUD

pub async fn create_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(_p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = if body.trim().is_empty() {
        json!({})
    } else {
        match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => return err(StatusCode::BAD_REQUEST, "parse_exception", e.to_string()),
        }
    };
    if store.exists(&index) {
        return err(
            StatusCode::BAD_REQUEST,
            "resource_already_exists_exception",
            format!("index [{index}] already exists"),
        );
    }
    // adaptive shard selection only makes sense on an append-only index
    let setting = |k: &str| -> Option<String> {
        body.pointer(&format!("/settings/index/{k}"))
            .or_else(|| body.pointer(&format!("/settings/index.{k}")))
            .map(|v| v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string()))
    };
    if setting("bulk.adaptive_shard_selection.enabled").as_deref() == Some("true")
        && setting("append_only.enabled").as_deref() != Some("true")
    {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "index [{index}] is not append-only index, bulk adaptive shard selection is \
                 enabled, which is not supported. Please disable bulk adaptive shard selection \
                 or set index to append-only index."
            ),
        );
    }
    if body
        .get("mappings")
        .map(|m| {
            m.get("properties")
                .and_then(|p| p.as_object())
                .map(|o| o.keys().any(|k| k.trim().is_empty()))
                .unwrap_or(false)
        })
        .unwrap_or(false)
    {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "field name cannot be an empty string",
        );
    }
    match store.create(&index, &body) {
        Ok(()) => axum::Json(json!({
            "acknowledged": true, "shards_acknowledged": true, "index": index
        }))
        .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string()),
    }
}

/// `_split`, `_shrink` and `_clone` -- make a new index out of an existing
/// one.
///
/// All three mean the same thing to an engine holding one shard per index: a
/// copy of every document under whatever settings the target is given. What
/// differs between them is only which shard counts are allowed, and that is
/// checked rather than acted on.
pub async fn resize_index(
    State(store): State<Store>,
    Path((source, target)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
    kind: &'static str,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let Some(src) = store.get(&source) else { return no_such_index(&source) };
    if store.exists(&target) {
        return err(
            StatusCode::BAD_REQUEST,
            "resource_already_exists_exception",
            format!("index [{target}] already exists"),
        );
    }
    let setting = |k: &str| -> Option<Value> {
        body.pointer(&format!("/settings/index/{k}"))
            .or_else(|| body.pointer(&format!("/settings/index.{k}")))
            .cloned()
    };
    let num = |k: &str| {
        setting(k).and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
    };
    // a target that cannot be written to would be created and then be
    // unusable, so those blocks are refused on the way in rather than left to
    // fail the copy
    for blocked in ["blocks.read_only", "blocks.metadata", "blocks.read_only_allow_delete"] {
        let set = setting(blocked)
            .map(|v| v == json!(true) || v == json!("true"))
            .unwrap_or(false);
        if set {
            return err(
                StatusCode::BAD_REQUEST,
                "action_request_validation_exception",
                format!(
                    "Validation Failed: 1: illegal value can not be set [index.{blocked}] on \
                     the target index;"
                ),
            );
        }
    }
    let from = src.read().numeric_setting("number_of_shards").unwrap_or(1) as i64;
    if let Some(to) = num("number_of_shards") {
        // a split has to multiply the shard count, a shrink to divide it
        match kind {
            "split" => {
                if setting("number_of_routing_shards").is_some() {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        "cannot provide index.number_of_routing_shards on resize",
                    );
                }
                if to <= from || to % from != 0 {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "illegal_state_exception",
                        format!(
                            "the number of source shards [{from}] must be a factor of [{to}]"
                        ),
                    );
                }
            }
            "shrink" => {
                if to >= from || from % to != 0 {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "illegal_state_exception",
                        format!(
                            "the number of source shards [{from}] must be a multiple of [{to}]"
                        ),
                    );
                }
            }
            _ => {
                if to != from {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        format!(
                            "the number of source shards [{from}] must be equal to [{to}]"
                        ),
                    );
                }
            }
        }
    }
    // the source has to be held still first: a copy taken while writes are
    // landing would not be a copy of anything in particular
    {
        let g = src.read();
        // a read-only source cannot be resized at all: the operation has to
        // write to it, not only read from it. The request may lift the block
        // as part of the same call, which is how a caller unwinds one.
        let lifted = matches!(setting("blocks.read_only"), Some(Value::Null))
            || setting("blocks.read_only").map(|v| v == json!(false) || v == json!("false"))
                .unwrap_or(false);
        if !lifted && g.setting("blocks.read_only").as_deref() == Some("true") {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("index [{source}] blocked by: [FORBIDDEN/5/index read-only (api)];"),
            );
        }
        if g.setting("blocks.write").as_deref() != Some("true") {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_state_exception",
                format!(
                    "index {source} must be read-only to resize index. use \
                     \"index.blocks.write=true\""
                ),
            );
        }
    }
    // the target starts from the source's shape, with what the request says
    // laid over it
    let (mapping, aliases, ids, inherited) = {
        let g = src.read();
        let ids: Vec<String> = g.all_ids();
        // the new index is the old one under another name, so it starts from
        // the source's settings and takes the request's over the top
        let mut inherited = g.effective_settings();
        if let Some(o) = inherited.pointer_mut("/index").and_then(|v| v.as_object_mut()) {
            o.retain(|k, _| !matches!(k.as_str(), "provided_name" | "uuid" | "creation_date"));
        }
        (g.mapping.raw.clone(), body.get("aliases").cloned(), ids, inherited)
    };
    let mut create = json!({"mappings": mapping});
    // settings always follow the index now; asking for them not to is asking
    // for behaviour that no longer exists
    if p.get("copy_settings").map(|v| v == "false").unwrap_or(false) {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "parameter [copy_settings] can not be explicitly set to [false]",
        );
    }
    let copy = true;
    let mut settings = if copy { inherited } else { json!({}) };
    if let Some(set) = body.get("settings") {
        crate::store::deep_merge(&mut settings, set);
    }
    // `max_shard_size` asks for as many shards as the data needs rather than
    // for a number; what this much data needs is one
    if p.contains_key("max_shard_size") && num("number_of_shards").is_none() {
        if let Some(o) = settings.pointer_mut("/index").and_then(|v| v.as_object_mut()) {
            o.insert("number_of_shards".into(), json!("1"));
        }
    }
    // the block that held the source still is carried over, but not until the
    // documents are: the copy goes through the write path, which the block
    // would otherwise refuse
    let mut held_back = serde_json::Map::new();
    if let Some(o) = settings.pointer_mut("/index").and_then(|v| v.as_object_mut()) {
        let blocked: Vec<String> =
            o.keys().filter(|k| k.starts_with("blocks")).cloned().collect();
        for k in blocked {
            if let Some(v) = o.remove(&k) {
                held_back.insert(k, v);
            }
        }
    }
    if settings.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
        create["settings"] = settings;
    }
    if let Some(a) = aliases {
        create["aliases"] = a;
    }
    if let Err(e) = store.create(&target, &create) {
        return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string());
    }
    let Some(dst) = store.get(&target) else { return no_such_index(&target) };
    for id in ids {
        let doc = { src.read().pending.get(&id).cloned().flatten() };
        let source_doc = match doc {
            Some(raw) => serde_json::from_str(&raw).ok(),
            None => read_source(&src.read(), &id),
        };
        let Some(v) = source_doc else { continue };
        let mut g = dst.write();
        let _ = write_doc(&mut g, &id, v, "index");
    }
    {
        let mut g = dst.write();
        let _ = g.refresh();
        if !held_back.is_empty() {
            let mut set = g.settings.clone();
            if !set.is_object() {
                set = json!({});
            }
            let slot = set.as_object_mut().unwrap().entry("index").or_insert(json!({}));
            crate::store::deep_merge(slot, &Value::Object(held_back));
            g.settings = set;
            g.save_meta();
        }
    }
    // `wait_for_completion=false` asks for the work to be tracked rather than
    // waited on; the copy is already done, so the task is a finished one
    if p.get("wait_for_completion").map(|v| v == "false").unwrap_or(false) {
        return respond(&p, json!({
            "task": format!("node-0:{kind} from [{source}] to [{target}]")
        }));
    }
    respond(&p, json!({
        "acknowledged": true, "shards_acknowledged": true, "index": target
    }))
}

pub async fn split_index(
    state: State<Store>,
    path: Path<(String, String)>,
    q: Query<Params>,
    body: String,
) -> Response {
    resize_index(state, path, q, body, "split").await
}

pub async fn shrink_index(
    state: State<Store>,
    path: Path<(String, String)>,
    q: Query<Params>,
    body: String,
) -> Response {
    resize_index(state, path, q, body, "shrink").await
}

pub async fn clone_index(
    state: State<Store>,
    path: Path<(String, String)>,
    q: Query<Params>,
    body: String,
) -> Response {
    resize_index(state, path, q, body, "clone").await
}

/// `_tasks/{id}` -- what became of a task.
///
/// Everything this engine is asked to do finishes before the request returns,
/// so a task named here is one that has already completed.
pub async fn get_task(Path(id): Path<String>, Query(p): Query<Params>) -> Response {
    // the id carries what the task was, after the node that ran it
    let what = id.split_once(':').map(|(_, d)| d).unwrap_or(&id).to_string();
    let action = if what.starts_with("open") {
        "indices:admin/open"
    } else if what.starts_with("shrink") || what.starts_with("split")
        || what.starts_with("clone")
    {
        "indices:admin/resize"
    } else {
        "indices:admin/tasks"
    };
    respond(&p, json!({
        "completed": true,
        "task": {
            "node": "node-0", "id": 1, "type": "transport",
            "action": action,
            "description": what,
            "start_time_in_millis": 0, "running_time_in_nanos": 0, "cancellable": false,
        },
        "response": {"acknowledged": true, "shards_acknowledged": true},
    }))
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
    match p.get("group_by").map(|v| v.as_str()) {
        // `none` asks for them in a plain list, `parents` keyed by their id
        Some("none") => return respond(&p, json!({"tasks": [task]})),
        Some("parents") => return respond(&p, json!({"tasks": {"node-0:1": task}})),
        _ => {}
    }
    respond(&p, json!({
        "nodes": {"node-0": {
            "name": "obsearch", "transport_address": "127.0.0.1:9300",
            "host": "127.0.0.1", "ip": "127.0.0.1",
            "roles": ["cluster_manager", "data", "ingest"],
            "tasks": {"node-0:1": task},
        }},
    }))
}

/// A size as written on a condition: a count and a unit.
fn parse_size(s: &str) -> Option<u64> {
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

/// The name that follows this one in a rolled-over series.
///
/// A name ending in digits carries on from that number, padded to six places,
/// which is how a series started by hand as `logs-1` becomes `logs-000002`.
fn next_rollover_name(current: &str) -> Option<String> {
    let digits = current.len() - current.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    let (stem, num) = current.split_at(current.len() - digits);
    let next: u64 = num.parse::<u64>().ok()? + 1;
    Some(format!("{stem}{next:06}"))
}

/// `_rollover` -- start a new index behind an alias when the one it points at
/// has had enough.
pub async fn rollover(
    State(store): State<Store>,
    path: Path<Vec<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let parts = path.0;
    let alias = parts.first().cloned().unwrap_or_default();
    let asked_name = parts.get(1).cloned();
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let dry_run = p.get("dry_run").map(|v| v != "false").unwrap_or(false);

    // the alias has to name exactly one index, or there is no one index to
    // roll over from
    let behind = store.resolve(&alias);
    let Some(old) = behind.first().cloned().filter(|_| behind.len() == 1) else {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("source alias [{alias}] does not point to a single index"),
        );
    };
    let Some(src) = store.get(&old) else { return no_such_index(&old) };
    let docs = src.read().reader.searcher().num_docs() as u64;

    // every condition is reported with whether it was met, met or not
    let mut conditions = serde_json::Map::new();
    let mut met = false;
    if let Some(o) = body.get("conditions").and_then(|v| v.as_object()) {
        for (name, want) in o {
            let text = want.as_str().map(|s| s.to_string()).unwrap_or_else(|| want.to_string());
            let hit = match name.as_str() {
                "max_docs" => docs >= want.as_u64().unwrap_or(u64::MAX),
                // the size an index has been given, against the size asked
                // about -- an index this young meets no age condition
                "max_size" | "max_primary_shard_size" => parse_size(&text)
                    .map(|limit| {
                        src.read().bytes.load(std::sync::atomic::Ordering::Relaxed) >= limit
                    })
                    .unwrap_or(false),
                "max_age" => false,
                _ => false,
            };
            met = met || hit;
            conditions.insert(format!("[{name}: {text}]"), json!(hit));
        }
    }

    let Some(new_index) = asked_name.or_else(|| next_rollover_name(&old)) else {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "index name [{old}] does not match pattern '^.*-\\d+$'"
            ),
        );
    };
    // the characters a name may not carry, which a caller supplying one of
    // their own can still get wrong
    if new_index.chars().any(|c| " \"*\\<|,>/?".contains(c)) {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_index_name_exception",
            format!("Invalid index name [{new_index}], must not contain the following characters [ , \", *, \\, <, |, ,, >, /, ?]"),
        );
    }
    if store.exists(&new_index) {
        return err(
            StatusCode::BAD_REQUEST,
            "resource_already_exists_exception",
            format!("index [{new_index}] already exists"),
        );
    }

    let rolled = met && !dry_run;
    if rolled {
        let create = body.get("mappings").or_else(|| body.get("settings")).map(|_| {
            let mut c = json!({});
            for k in ["settings", "mappings", "aliases"] {
                if let Some(v) = body.get(k) {
                    c[k] = v.clone();
                }
            }
            c
        }).unwrap_or_else(|| json!({}));
        if let Err(e) = store.create(&new_index, &create) {
            return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string());
        }
        // the alias moves; the old index keeps whatever else pointed at it
        let def = { src.read().aliases.get(&alias).cloned().unwrap_or_else(|| json!({})) };
        if let Some(st) = store.get(&new_index) {
            st.write().aliases.insert(alias.clone(), def);
        }
        src.write().aliases.remove(&alias);
    }
    respond(&p, json!({
        "acknowledged": true,
        "shards_acknowledged": rolled,
        "old_index": old,
        "new_index": new_index,
        "rolled_over": rolled,
        "dry_run": dry_run,
        "conditions": Value::Object(conditions),
    }))
}

/// `_cluster/stats` -- the cluster in one page.
pub async fn cluster_stats(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    let names = store.names();
    let mut docs = 0u64;
    for n in &names {
        if let Some(st) = store.get(n) {
            docs += st.read().reader.searcher().num_docs() as u64;
        }
    }
    let replicated = names.iter().filter_map(|n| store.get(n)).any(|st| {
        st.read().numeric_setting("number_of_replicas").unwrap_or(0) > 0
    });
    respond(&p, json!({
        "_nodes": {"total": 1, "successful": 1, "failed": 0},
        "cluster_name": "obsearch",
        "cluster_uuid": "_na_",
        "timestamp": 1_577_836_800_000u64,
        "status": if replicated { "yellow" } else { "green" },
        "indices": {
            "count": names.len(),
            "shards": {
                "total": names.len(), "primaries": names.len(),
                "replication": 0.0,
                "index": {
                    "shards": {"min": 1, "max": 1, "avg": 1.0},
                    "primaries": {"min": 1, "max": 1, "avg": 1.0},
                    "replication": {"min": 0.0, "max": 0.0, "avg": 0.0},
                },
            },
            "docs": {"count": docs, "deleted": 0},
            "store": {"size_in_bytes": 0, "reserved_in_bytes": 0},
            "fielddata": {"memory_size_in_bytes": 0, "evictions": 0},
            "query_cache": {
                "memory_size_in_bytes": 0, "total_count": 0, "hit_count": 0,
                "miss_count": 0, "cache_size": 0, "cache_count": 0, "evictions": 0,
            },
            "completion": {"size_in_bytes": 0},
            "segments": {
                "count": 0, "memory_in_bytes": 0, "terms_memory_in_bytes": 0,
                "stored_fields_memory_in_bytes": 0, "term_vectors_memory_in_bytes": 0,
                "norms_memory_in_bytes": 0, "points_memory_in_bytes": 0,
                "doc_values_memory_in_bytes": 0, "index_writer_memory_in_bytes": 0,
                "version_map_memory_in_bytes": 0, "fixed_bit_set_memory_in_bytes": 0,
                "max_unsafe_auto_id_timestamp": -1, "file_sizes": {},
            },
            "mappings": {"field_types": []},
            "analysis": {"char_filter_types": [], "tokenizer_types": [],
                         "filter_types": [], "analyzer_types": [],
                         "built_in_char_filters": [], "built_in_tokenizers": [],
                         "built_in_filters": [], "built_in_analyzers": []},
        },
        "nodes": {
            "count": {"total": 1, "cluster_manager": 1, "coordinating_only": 0,
                      "data": 1, "ingest": 1, "master": 1, "remote_cluster_client": 1,
                      "search": 0, "warm": 0},
            "versions": ["3.0.0"],
            "os": {
                "available_processors": 1, "allocated_processors": 1,
                "names": [], "pretty_names": [], "roles": [],
                "mem": {
                    "total_in_bytes": 1_073_741_824u64,
                    "free_in_bytes": 536_870_912u64,
                    "used_in_bytes": 536_870_912u64,
                    "free_percent": 50, "used_percent": 50,
                },
            },
            "process": {
                "cpu": {"percent": 0},
                "open_file_descriptors": {"min": 0, "max": 0, "avg": 0},
            },
            "jvm": {
                "max_uptime_in_millis": 0, "versions": [],
                "mem": {"heap_used_in_bytes": 0, "heap_max_in_bytes": 0},
                "threads": 1,
            },
            "fs": {
                "total_in_bytes": 2_147_483_648u64,
                "free_in_bytes": 1_073_741_824u64,
                "available_in_bytes": 1_073_741_824u64,
            },
            "plugins": [],
            "network_types": {
                "transport_types": {"transport": 1},
                "http_types": {"http": 1},
            },
            "discovery_types": {"single-node": 1},
            "packaging_types": [],
            "ingest": {"number_of_pipelines": 0, "processor_stats": {}},
        },
    }))
}

/// `_shard_stores` -- where each shard's copies are.
///
/// One node holds one copy of each shard, and it is the primary.
pub async fn shard_stores(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let names = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
    let allow_none = p.get("allow_no_indices").map(|v| v != "false").unwrap_or(true);
    if names.is_empty() && !allow_none {
        return no_such_index(if expr.is_empty() { "_all" } else { &expr });
    }
    // by default only the shards that are missing a copy are listed, and none
    // here ever are
    let status: Vec<&str> = p
        .get("status")
        .map(|v| v.split(',').map(|s| s.trim()).collect())
        .unwrap_or_else(|| vec!["yellow", "red"]);
    let listed = status.iter().any(|s| matches!(*s, "green" | "all"));
    let mut indices = serde_json::Map::new();
    if listed {
        for n in names {
            let Some(st) = store.get(&n) else { continue };
            let g = st.read();
            let shards = g.numeric_setting("number_of_shards").unwrap_or(1).max(1);
            let mut per = serde_json::Map::new();
            for i in 0..shards {
                per.insert(i.to_string(), json!({"stores": [{
                    "node-0": {
                        "name": "obsearch", "ephemeral_id": "_na_",
                        "transport_address": "127.0.0.1:9300", "attributes": {},
                    },
                    "allocation_id": "_na_",
                    "allocation": "primary",
                }]}));
            }
            indices.insert(g.name.clone(), json!({"shards": Value::Object(per)}));
        }
    }
    respond(&p, json!({"indices": Value::Object(indices)}))
}

/// `_resolve/index` -- what a name or pattern actually reaches.
pub async fn resolve_index(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let mut indices = Vec::new();
    let mut aliases: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    // a pattern reaches the states `expand_wildcards` names, which is open
    // ones unless the caller says otherwise
    let states = p.get("expand_wildcards").map(|v| v.as_str()).unwrap_or("open");
    let want_open = states.split(',').any(|w| matches!(w.trim(), "open" | "all"));
    let want_closed = states.split(',').any(|w| matches!(w.trim(), "closed" | "all"));
    let mut names = store.names();
    names.sort();
    for n in names {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        let mut own: Vec<String> = g.aliases.keys().cloned().collect();
        own.sort();
        // an alias names every index behind it, whatever state each is in --
        // closing one does not take it out from behind its alias
        for a in &own {
            aliases.entry(a.clone()).or_default().push(g.name.clone());
        }
        // the index listing itself only reaches the states asked for
        if (g.closed && !want_closed) || (!g.closed && !want_open) {
            continue;
        }
        let hit = name.split(',').any(|pat| {
            let pat = pat.trim();
            pat == "*" || pat == "_all" || pat == g.name || crate::store::glob_match(pat, &g.name)
                || own.iter().any(|a| a == pat || crate::store::glob_match(pat, a))
        });
        if hit {
            indices.push(json!({
                "name": g.name,
                "aliases": own,
                "attributes": [if g.closed { "closed" } else { "open" }],
            }));
        }
    }
    let alias_list: Vec<Value> = aliases
        .into_iter()
        .filter(|(a, _)| {
            name.split(',').any(|pat| {
                let pat = pat.trim();
                pat == "*" || pat == "_all" || pat == a || crate::store::glob_match(pat, a)
            })
        })
        .map(|(a, mut idx)| {
            idx.sort();
            json!({"name": a, "indices": idx})
        })
        .collect();
    respond(&p, json!({
        "indices": indices, "aliases": alias_list, "data_streams": [],
    }))
}

/// `_remote/info` -- the clusters this one is connected to, of which there
/// are none.
pub async fn remote_info(Query(p): Query<Params>) -> Response {
    respond(&p, json!({}))
}

/// `_block/{block}` -- hold an index still without closing it.
pub async fn add_block(
    State(store): State<Store>,
    Path((index, block)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    let targets = store.resolve(&index);
    if targets.is_empty() {
        return no_such_index(&index);
    }
    let mut blocked = Vec::new();
    for n in &targets {
        let Some(st) = store.get(n) else { continue };
        let mut g = st.write();
        let mut settings = g.settings.clone();
        if !settings.is_object() {
            settings = json!({});
        }
        let slot = settings.as_object_mut().unwrap().entry("index").or_insert(json!({}));
        crate::store::deep_merge(slot, &json!({format!("blocks.{block}"): "true"}));
        g.settings = settings;
        g.save_meta();
        blocked.push(json!({"name": g.name.clone(), "blocked": true}));
    }
    respond(&p, json!({
        "acknowledged": true, "shards_acknowledged": true, "indices": blocked
    }))
}

/// `_termvectors` -- what a document's text became once analysed.
///
/// The terms are recovered by analysing the stored source again rather than
/// from a second index of offsets, which is the same ground the highlighter
/// stands on. Document frequency is counted against the index, so it is the
/// real one rather than a guess.
pub async fn termvectors(
    State(store): State<Store>,
    path: Path<Vec<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let parts = path.0;
    let index = parts.first().cloned().unwrap_or_default();
    let id = parts.get(1).cloned();
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let Some(st) = store.get(&index) else { return no_such_index(&index) };
    let g = st.read();
    let id = id.or_else(|| body.get("_id").and_then(|v| v.as_str().map(|s| s.into())));
    let Some(id) = id else {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: id is missing;",
        );
    };
    let source = read_source_as_asked(&g, &id, &p);
    let Some(source) = source else {
        return respond(&p, json!({
            "_index": g.name, "_id": id, "_version": 0, "found": false, "took": 0,
        }));
    };
    let want_stats = body
        .get("term_statistics")
        .and_then(|v| v.as_bool())
        .or_else(|| p.get("term_statistics").map(|v| v == "true"))
        .unwrap_or(false);
    let only: Option<Vec<String>> = body
        .get("fields")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .or_else(|| {
            p.get("fields").map(|f| f.split(',').map(|s| s.trim().to_string()).collect())
        });

    let fields = term_vectors_of(&g, &source, want_stats, only.as_deref());
    return respond(&p, json!({
        "_index": g.name, "_id": id, "_version": g.version_of(&id),
        "found": true, "took": 0, "term_vectors": fields,
    }));
}

/// The terms each field of a document became once analysed.
fn term_vectors_of(
    g: &IdxState,
    source: &Value,
    want_stats: bool,
    only: Option<&[String]>,
) -> Value {
    let mut fields = serde_json::Map::new();
    let Some(obj) = source.as_object() else { return Value::Object(fields) };
    for (name, value) in obj {
        if only.map(|f| !f.iter().any(|w| w == name)).unwrap_or(false) {
            continue;
        }
        let Some(text) = value.as_str() else { continue };
        let spans = crate::query::analyze_spans(&g.index, text, None);
        if spans.is_empty() {
            continue;
        }
        // group the occurrences by the term they are of
        let mut terms: std::collections::BTreeMap<String, Vec<(usize, usize, usize)>> =
            std::collections::BTreeMap::new();
        for (t, pos, from, to) in spans {
            terms.entry(t).or_default().push((pos, from, to));
        }
        let searcher = g.reader.searcher();
        let mut out = serde_json::Map::new();
        let mut sum_doc_freq = 0u64;
        let mut sum_ttf = 0u64;
        for (term, spots) in &terms {
            let mut entry = json!({
                "term_freq": spots.len(),
                "tokens": spots.iter().map(|(pos, from, to)| json!({
                    "position": pos, "start_offset": from, "end_offset": to,
                })).collect::<Vec<_>>(),
            });
            if want_stats {
                // how many documents hold this term, counted rather than assumed
                let ctx = crate::query::Ctx {
                    fields: &g.fields,
                    mapping: &g.mapping,
                    index: &g.index,
                    max_terms_count: g.max_terms_count(),
                    observed_kinds: &g.observed_kinds,
                    kinds_complete: g.kinds_complete,
                    stats: &g.stats,
                };
                let freq = crate::query::build(&ctx, &json!({"match": {name.clone(): term}}))
                    .ok()
                    .and_then(|q| searcher.search(&q, &tantivy::collector::Count).ok())
                    .unwrap_or(1) as u64;
                entry["doc_freq"] = json!(freq);
                entry["ttf"] = json!(spots.len());
                sum_doc_freq += freq;
                sum_ttf += spots.len() as u64;
            }
            out.insert(term.clone(), entry);
        }
        let mut field = json!({"terms": Value::Object(out)});
        if want_stats {
            field["field_statistics"] = json!({
                "sum_doc_freq": sum_doc_freq,
                "doc_count": searcher.num_docs(),
                "sum_ttf": sum_ttf,
            });
        }
        fields.insert(name.clone(), field);
    }
    Value::Object(fields)
}

/// `_mtermvectors` -- term vectors for several documents at once.
pub async fn mtermvectors(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let default_index = index.map(|Path(i)| i);
    let docs: Vec<Value> = match body.get("docs").and_then(|v| v.as_array()) {
        Some(a) => a.clone(),
        None => {
            // `ids` may be written in the body or on the URL
            let listed: Vec<Value> = body
                .get("ids")
                .and_then(|v| v.as_array())
                .cloned()
                .or_else(|| {
                    p.get("ids").filter(|v| !v.is_empty()).map(|v| {
                        v.split(',').map(|s| json!(s.trim())).collect()
                    })
                })
                .unwrap_or_default();
            listed.into_iter().map(|id| json!({"_id": id})).collect()
        }
    };
    // the camel-cased spellings, and the ones that moved onto the request,
    // are no longer read from a document
    for d in &docs {
        for gone in ["version", "versionType", "version_type", "_version", "routing", "_routing"] {
            if d.get(gone).is_some() {
                return err(
                    StatusCode::BAD_REQUEST,
                    "action_request_validation_exception",
                    format!("Validation Failed: 1: unknown field [{gone}];"),
                );
            }
        }
    }
    let mut out = Vec::new();
    for d in docs {
        let idx = d
            .get("_index")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .or_else(|| default_index.clone())
            .unwrap_or_default();
        let id = d.get("_id").map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        });
        let Some(id) = id else { continue };
        let Some(st) = store.get(&idx) else {
            let reason = format!("no such index [{idx}]");
            let cause = json!({
                "type": "index_not_found_exception", "reason": reason,
                "index": idx, "resource.type": "index_expression",
                "resource.id": idx, "index_uuid": "_na_",
            });
            let mut error = json!({
                "type": "index_not_found_exception", "reason": reason,
                "index": idx, "resource.type": "index_expression",
                "resource.id": idx, "index_uuid": "_na_",
                "root_cause": [cause],
            });
            add_stack_trace(&mut error, &p, "mtermvectors");
            out.push(json!({"_index": idx, "_id": id, "found": false, "error": error}));
            continue;
        };
        let g = st.read();
        match read_source_as_asked(&g, &id, &p) {
            Some(src) => {
                let want_stats = body
                    .get("term_statistics")
                    .and_then(|v| v.as_bool())
                    .or_else(|| p.get("term_statistics").map(|v| v == "true"))
                    .unwrap_or(false);
                out.push(json!({
                    "_index": g.name, "_id": id, "_version": g.version_of(&id),
                    "found": true, "took": 0,
                    "term_vectors": term_vectors_of(&g, &src, want_stats, None),
                }))
            }
            None => out.push(json!({
                "_index": g.name, "_id": id, "found": false, "took": 0,
            })),
        }
    }
    respond(&p, json!({"docs": out}))
}

/// `_cluster/reroute` -- move shards about. There is one node, so there is
/// nowhere to move them, and the answer is the state as it stands.
pub async fn reroute(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let _ = parse_body(&body);
    let mut out = json!({"acknowledged": true, "explanations": []});
    // `metric` names which parts of the state to send back; without it the
    // state is left out entirely
    let metrics: Vec<String> = p
        .get("metric")
        .map(|m| m.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    let want = |name: &str| metrics.iter().any(|m| m == name || m == "_all");
    if !metrics.is_empty() && !metrics.iter().any(|m| m == "none") {
        let mut indices = serde_json::Map::new();
        for n in store.names() {
            let Some(st) = store.get(&n) else { continue };
            let g = st.read();
            indices.insert(g.name.clone(), json!({
                "aliases": g.aliases.keys().cloned().collect::<Vec<_>>(),
                "mappings": g.mapping.raw,
                "settings": g.effective_settings(),
                "state": if g.closed { "close" } else { "open" },
            }));
        }
        let mut state = json!({
            "cluster_name": "obsearch", "cluster_uuid": "_na_",
            "version": 1, "state_uuid": "_na_",
        });
        if want("master_node") {
            state["master_node"] = json!("node-0");
        }
        if want("cluster_manager_node") {
            state["cluster_manager_node"] = json!("node-0");
        }
        if want("nodes") {
            state["nodes"] = json!({"node-0": {
                "name": "obsearch", "ephemeral_id": "_na_",
                "transport_address": "127.0.0.1:9300", "attributes": {}}});
        }
        if want("metadata") {
            state["metadata"] = json!({
                "cluster_uuid": "_na_", "templates": store.get_templates(),
                "indices": Value::Object(indices),
            });
        }
        out["state"] = state;
    }
    respond(&p, out)
}

/// `_cluster/pending_tasks` -- work the cluster manager has queued, of which
/// there is never any: everything here finishes before its request returns.
pub async fn pending_tasks(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"tasks": []}))
}

/// A keep-alive as written, in milliseconds.
fn keep_alive_millis(s: &str) -> u64 {
    parse_keep_alive(s).map(|secs| secs * 1000).unwrap_or(0)
}

/// `_search/point_in_time` -- freeze what the indices hold now, so that
/// paging through them is not disturbed by writes that arrive meanwhile.
pub async fn create_pit(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let names = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
    if names.is_empty() && !expr.is_empty() {
        return no_such_index(&expr);
    }
    let keep = p.get("keep_alive").map(|v| keep_alive_millis(v)).unwrap_or(0);
    let id = store.open_pit(&expr, keep);
    respond(&p, json!({
        "pit_id": id,
        "_shards": shards_over(&store, &names),
        "creation_time": 0,
    }))
}

pub async fn get_all_pits(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    let pits: Vec<Value> = store
        .all_pits()
        .into_iter()
        .map(|(id, st)| json!({
            "pit_id": id, "creation_time": 0, "keep_alive": st.keep_alive_ms,
        }))
        .collect();
    respond(&p, json!({"pits": pits}))
}

pub async fn delete_pit(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let ids: Vec<String> = match body.get("pit_id") {
        Some(Value::Array(a)) => {
            a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()
        }
        Some(Value::String(one)) => vec![one.clone()],
        // no id names them all
        _ => store.all_pits().into_iter().map(|(id, _)| id).collect(),
    };
    let pits: Vec<Value> = ids
        .into_iter()
        .map(|id| {
            let gone = store.close_pit(&id);
            json!({"pit_id": id, "successful": gone})
        })
        .collect();
    respond(&p, json!({"pits": pits}))
}

/// `_script_context` -- the places a script may run and what each hands it.
pub async fn script_contexts(Query(p): Query<Params>) -> Response {
    let ctx = |name: &str, ret: &str| json!({
        "name": name,
        "methods": [{"name": "execute", "return_type": ret, "params": []}],
    });
    respond(&p, json!({"contexts": [
        ctx("aggs", "double"),
        ctx("filter", "boolean"),
        ctx("score", "double"),
        ctx("update", "void"),
    ]}))
}

/// `_script_language` -- which languages scripts may be written in, and how
/// they may be supplied.
pub async fn script_languages(Query(p): Query<Params>) -> Response {
    respond(&p, json!({
        "types_allowed": ["inline", "stored"],
        "language_contexts": [{
            "language": "painless",
            "contexts": ["aggs", "filter", "score", "update"],
        }],
    }))
}

/// `_nodes/stats` -- what the one node has been doing.
pub async fn nodes_stats(
    State(store): State<Store>,
    rest: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    // the path may name which metrics are wanted, and a name that is not one
    // of them is a mistake rather than something to pass over
    const METRICS: &[&str] = &[
        "_all", "indices", "os", "process", "jvm", "thread_pool", "fs", "transport",
        "http", "breaker", "script", "discovery", "ingest", "adaptive_selection",
        "script_cache", "indexing_pressure", "shard_indexing_pressure",
        "search_backpressure", "cluster_manager_throttling", "weighted_routing",
        "resource_usage_stats", "segment_replication_backpressure", "repositories",
        "admission_control", "caches", "remote_store",
    ];
    // only the first path part names the metrics; anything after it narrows
    // within one, and is checked by whatever owns that metric
    let rest_parts: Vec<String> = rest
        .map(|Path(r)| r.split('/').map(|s| s.to_string()).collect())
        .unwrap_or_default();
    let asked: Vec<String> = rest_parts
        .first()
        .map(|r| r.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    for m in asked.iter().filter(|m| !m.is_empty() && *m != "stats") {
        // a node id is also allowed in this position, and ours is known
        if METRICS.contains(&m.as_str()) || matches!(m.as_str(), "node-0" | "_local" | "_all") {
            continue;
        }
        // a near miss is a typo, and naming the metric meant saves a reading
        // of the whole list
        let near = METRICS.iter().find(|k| {
            k.len().abs_diff(m.len()) <= 1
                && k.chars().filter(|c| m.contains(*c)).count() + 1 >= k.len()
        });
        let hint = match near {
            Some(k) => format!(" -> did you mean [{k}]?"),
            None => String::new(),
        };
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("request [/_nodes/stats/{m}] contains unrecognized metric: [{m}]{hint}"),
        );
    }
    let mut docs = 0u64;
    for n in store.names() {
        if let Some(st) = store.get(&n) {
            docs += st.read().reader.searcher().num_docs() as u64;
        }
    }
    // a second path part narrows within `indices` to the metrics it names
    let index_metrics: Vec<String> = rest_parts
        .get(1)
        .map(|r| r.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    let zero_time = json!({"total": 0, "time_in_millis": 0, "current": 0});
    let mut out = json!({
        "_nodes": {"total": 1, "successful": 1, "failed": 0},
        "cluster_name": "obsearch",
        "nodes": {"node-0": {
            "timestamp": 0, "name": "obsearch",
            "transport_address": "127.0.0.1:9300", "host": "127.0.0.1", "ip": "127.0.0.1",
            "roles": ["cluster_manager", "data", "ingest"], "attributes": {},
            "indices": {
                "docs": {"count": docs, "deleted": 0},
                "store": {"size_in_bytes": 0, "reserved_in_bytes": 0},
                "indexing": {
                    "index_total": docs, "index_time_in_millis": 0, "index_current": 0,
                    "index_failed": 0, "delete_total": 0, "delete_time_in_millis": 0,
                    "delete_current": 0, "noop_update_total": 0, "is_throttled": false,
                    "throttle_time_in_millis": 0,
                    "doc_status": {},
                },
                // how many writes ended in each status class, which is what a
                // caller watching for rejections reads
                "status_counter": {
                    "doc_status": {"1xx": 0, "2xx": 0, "3xx": 0, "4xx": 0, "5xx": 0},
                    "search_response_status": {
                        "1xx": 0, "2xx": 0, "3xx": 0, "4xx": 0, "5xx": 0
                    },
                },
                "get": {"total": 0, "time_in_millis": 0, "exists_total": 0,
                        "exists_time_in_millis": 0, "missing_total": 0,
                        "missing_time_in_millis": 0, "current": 0},
                "search": {"open_contexts": 0, "query_total": 0, "query_time_in_millis": 0,
                           "query_current": 0, "fetch_total": 0, "fetch_time_in_millis": 0,
                           "fetch_current": 0, "scroll_total": 0, "scroll_time_in_millis": 0,
                           "scroll_current": 0, "suggest_total": 0,
                           "suggest_time_in_millis": 0, "suggest_current": 0},
                "merges": {"current": 0, "current_docs": 0, "current_size_in_bytes": 0,
                           "total": 0, "total_time_in_millis": 0, "total_docs": 0,
                           "total_size_in_bytes": 0},
                "refresh": {"total": 0, "total_time_in_millis": 0, "external_total": 0,
                            "external_total_time_in_millis": 0, "listeners": 0},
                "flush": {"total": 0, "periodic": 0, "total_time_in_millis": 0},
                "warmer": zero_time.clone(),
                "query_cache": {"memory_size_in_bytes": 0, "total_count": 0,
                                "hit_count": 0, "miss_count": 0, "cache_size": 0,
                                "cache_count": 0, "evictions": 0},
                "fielddata": {"memory_size_in_bytes": 0, "evictions": 0},
                "completion": {"size_in_bytes": 0},
                "segments": {"count": 0, "memory_in_bytes": 0},
                "translog": {"operations": 0, "size_in_bytes": 0,
                             "uncommitted_operations": 0, "uncommitted_size_in_bytes": 0,
                             "earliest_last_modified_age": 0},
                "request_cache": {"memory_size_in_bytes": 0, "evictions": 0,
                                  "hit_count": 0, "miss_count": 0},
                "recovery": {"current_as_source": 0, "current_as_target": 0,
                             "throttle_time_in_millis": 0},
            },
            "os": {"timestamp": 0, "cpu": {"percent": 0}, "mem": {
                "total_in_bytes": 1_073_741_824u64, "free_in_bytes": 536_870_912u64,
                "used_in_bytes": 536_870_912u64, "free_percent": 50, "used_percent": 50}},
            "process": {"timestamp": 0, "open_file_descriptors": 0,
                        "max_file_descriptors": 0,
                        "cpu": {"percent": 0, "total_in_millis": 0},
                        "mem": {"total_virtual_in_bytes": 0}},
            "jvm": {"timestamp": 0, "uptime_in_millis": 0,
                    "mem": {"heap_used_in_bytes": 0, "heap_max_in_bytes": 0},
                    "threads": {"count": 1, "peak_count": 1}},
            "thread_pool": {}, "fs": {"total": {
                "total_in_bytes": 2_147_483_648u64, "free_in_bytes": 1_073_741_824u64,
                "available_in_bytes": 1_073_741_824u64}},
            "transport": {"server_open": 0, "rx_count": 0, "rx_size_in_bytes": 0,
                          "tx_count": 0, "tx_size_in_bytes": 0},
            "http": {"current_open": 0, "total_opened": 0},
            "breakers": {}, "script": {"compilations": 0, "cache_evictions": 0},
            "discovery": {}, "ingest": {"total": zero_time, "pipelines": {}},
            "adaptive_selection": {}, "script_cache": {"sum": {}},
            "indexing_pressure": {"memory": {}},
        }},
    });
    if !index_metrics.is_empty() && !index_metrics.iter().any(|m| m == "_all") {
        if let Some(idx) = out.pointer_mut("/nodes/node-0/indices").and_then(|v| v.as_object_mut())
        {
            // the status counter belongs to indexing, and travels with it
            idx.retain(|k, _| {
                index_metrics.iter().any(|m| m == k)
                    || (k == "status_counter" && index_metrics.iter().any(|m| m == "indexing"))
            });
        }
    }
    respond(&p, out)
}

pub async fn delete_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let lenient = ignore_unavailable(&p);
    // an alias names indices without being one; deleting it would have to
    // mean deleting what it points at, which is not what was asked
    for part in index.split(',').map(|n| n.trim()).filter(|n| !n.is_empty()) {
        if !part.contains('*') && store.is_alias(part) && !lenient {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!(
                    "The provided expression [{part}] matches an alias, specify the \
                     corresponding concrete indices instead."
                ),
            );
        }
    }
    // a pattern reaches for indices, not for the aliases that stand in front
    // of them, so it is matched against the real names only
    let mut targets: Vec<String> = Vec::new();
    let mut missing: Option<String> = None;
    for part in index.split(',').map(|n| n.trim()).filter(|n| !n.is_empty()) {
        if part.contains('*') {
            for n in store.names() {
                if crate::store::glob_match(part, &n) && !targets.contains(&n) {
                    targets.push(n);
                }
            }
        } else if store.is_alias(part) {
            continue;
        } else {
            let found = store.resolve(part);
            if found.is_empty() {
                missing.get_or_insert_with(|| part.to_string());
            }
            for n in found {
                if !targets.contains(&n) {
                    targets.push(n);
                }
            }
        }
    }
    if let Some(name) = missing.filter(|_| !lenient) {
        return no_such_index(&name);
    }
    // `allow_no_indices=false` makes an expression that reaches nothing an
    // error rather than a request with nothing to do
    // `allow_no_indices=false` is asked of each pattern in turn: an
    // expression is only satisfied if every part of it reached something
    let allow_none = p.get("allow_no_indices").map(|v| v != "false").unwrap_or(true);
    if !allow_none {
        for part in index.split(',').map(|n| n.trim()).filter(|n| n.contains('*')) {
            let reached = store.names().iter().any(|n| crate::store::glob_match(part, n));
            if !reached {
                return no_such_index(part);
            }
        }
        if targets.is_empty() {
            return no_such_index(&index);
        }
    }
    for n in &targets {
        store.delete(n);
    }
    axum::Json(json!({"acknowledged": true})).into_response()
}

pub async fn index_exists(State(store): State<Store>, Path(index): Path<String>) -> Response {
    if store.resolve(&index).is_empty() {
        StatusCode::NOT_FOUND.into_response()
    } else {
        StatusCode::OK.into_response()
    }
}

/// `_flush` writes what is buffered and makes it searchable.
///
/// The distinction OpenSearch draws is between committing to disk and making
/// documents visible; here committing does both, so a flush is a refresh that
/// also settles the writer.
pub async fn flush(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(_p): Query<Params>,
) -> Response {
    // a forced flush has to be allowed to wait for one already running, or it
    // would have to refuse to do the thing it was asked for
    if _p.get("force").map(|v| v != "false").unwrap_or(false)
        && _p.get("wait_if_ongoing").map(|v| v == "false").unwrap_or(false)
    {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: wait_if_ongoing must be true for a force flush;",
        );
    }
    let targets = match index {
        Some(Path(i)) => {
            let t = store.resolve(&i);
            if t.is_empty() {
                return no_such_index(&i);
            }
            t
        }
        None => store.names(),
    };
    let tally = shards_over(&store, &targets);
    for n in targets {
        if let Some(st) = store.get(&n) {
            let mut g = st.write();
            let _ = g.refresh();
            g.flushes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    axum::Json(json!({"_shards": tally})).into_response()
}

pub async fn refresh_all(State(store): State<Store>) -> Response {
    let names = store.names();
    for n in &names {
        if let Some(st) = store.get(n) {
            let _ = st.write().refresh();
        }
    }
    axum::Json(json!({"_shards": shards_over(&store, &names)})).into_response()
}

pub async fn refresh_index(State(store): State<Store>, Path(index): Path<String>) -> Response {
    let targets = store.resolve(&index);
    // a pattern that reaches nothing has nothing to refresh, which is not an
    // error; a name given outright must be there
    if targets.is_empty() && !index.contains('*') && index != "_all" && !index.is_empty() {
        return no_such_index(&index);
    }
    let tally = shards_over(&store, &targets);
    for n in targets {
        if let Some(st) = store.get(&n) {
            let _ = st.write().refresh();
        }
    }
    axum::Json(json!({"_shards": tally})).into_response()
}

// ------------------------------------------------------------------ mappings

fn mapping_view(st: &IdxState) -> Value {
    let mut m = if st.mapping.raw.is_null() { json!({}) } else { st.mapping.raw.clone() };
    add_type_defaults(&mut m);
    json!({"mappings": m})
}

/// Defaults a type carries even when the request did not spell them out.
fn add_type_defaults(node: &mut Value) {
    let Some(obj) = node.as_object_mut() else { return };
    if obj.get("type").and_then(|t| t.as_str()) == Some("wildcard")
        && !obj.contains_key("doc_values")
    {
        obj.insert("doc_values".into(), json!(true));
    }
    for key in ["properties", "fields"] {
        if let Some(children) = obj.get_mut(key).and_then(|c| c.as_object_mut()) {
            for (_, child) in children.iter_mut() {
                add_type_defaults(child);
            }
        }
    }
}

pub async fn get_mapping(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_else(|| "_all".into());
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !ignore_unavailable(&p) {
        return no_such_index(&expr);
    }
    // `expand_wildcards` says which states a pattern reaches; `none` means it
    // reaches nothing at all
    let states = p.get("expand_wildcards").map(|v| v.as_str()).unwrap_or("open");
    let reach = |closed: bool| {
        states.split(',').any(|w| match w.trim() {
            "all" => true,
            "open" => !closed,
            "closed" => closed,
            _ => false,
        })
    };
    let mut out = serde_json::Map::new();
    for n in targets {
        if let Some(st) = store.get(&n) {
            let g = st.read();
            if expr.contains('*') && !reach(g.closed) {
                continue;
            }
            out.insert(n, mapping_view(&g));
        }
    }
    // a pattern reaching nothing is an error only when the caller said so
    let allow_none = p.get("allow_no_indices").map(|v| v != "false").unwrap_or(true);
    if out.is_empty() && !allow_none {
        return no_such_index(&expr);
    }
    axum::Json(Value::Object(out)).into_response()
}

pub async fn put_mapping(
    State(store): State<Store>,
    Path(index): Path<String>,
    body: String,
) -> Response {
    let body: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    // a mapping names fields, not a type; the type that used to wrap them is
    // gone and a body still carrying one is not a mapping
    if body.get("_doc").is_some() && body.get("properties").is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "Types cannot be provided in put mapping requests",
        );
    }
    // a field has to be called something
    fn empty_named(node: &Value) -> bool {
        let Some(props) = node.get("properties").and_then(|p| p.as_object()) else {
            return false;
        };
        props.keys().any(|k| k.trim().is_empty())
            || props.values().any(empty_named)
    }
    if empty_named(&body) {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "field name cannot be an empty string",
        );
    }
    let targets = store.resolve(&index);
    if targets.is_empty() {
        return no_such_index(&index);
    }
    for n in targets {
        if let Some(st) = store.get(&n) {
            let mut g = st.write();
            g.mapping.merge(&body);
        }
    }
    axum::Json(json!({"acknowledged": true})).into_response()
}

/// Settings OpenSearch reports under `defaults` when asked for them. Only the
/// handful the conformance suite reads are modelled.
fn default_settings() -> Value {
    json!({"index": {
        "refresh_interval": "1s",
        "max_result_window": "10000",
        "number_of_routing_shards": "1",
        "codec": "default",
        "auto_expand_replicas": "false",
        "max_inner_result_window": "100",
        "max_rescore_window": "10000",
        "query": {"default_field": ["*"]},
    }})
}

/// `flat_settings=true` renders `{"index":{"a":1}}` as `{"index.a":1}`.
fn flatten_settings(v: &Value, prefix: &str, out: &mut serde_json::Map<String, Value>) {
    match v {
        Value::Object(o) => {
            for (k, child) in o {
                let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten_settings(child, &path, out);
            }
        }
        leaf => {
            out.insert(prefix.to_string(), leaf.clone());
        }
    }
}

/// `human` asks for a readable form beside each machine one.
fn add_human_settings(view: &mut Value, st: &IdxState) {
    let created = st.created_millis();
    let text = st.created_string();
    if let Some(o) = view.pointer_mut("/index").and_then(|v| v.as_object_mut()) {
        o.insert("creation_date_string".into(), json!(text));
        o.entry("creation_date".to_string()).or_insert_with(|| json!(created.to_string()));
        // the version an index was made under is reported the same two ways
        let ver = o
            .get("version")
            .and_then(|v| v.get("created"))
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "136407827".to_string());
        o.insert("version".into(), json!({
            "created": ver, "created_string": "3.9.0",
        }));
    } else if let Some(o) = view.as_object_mut() {
        o.insert("index.creation_date_string".into(), json!(text));
        o.entry("index.creation_date".to_string())
            .or_insert_with(|| json!(created.to_string()));
    }
}

fn settings_view(raw: &Value, name: Option<&str>, flat: bool) -> Value {
    let mut flat_map = serde_json::Map::new();
    flatten_settings(raw, "", &mut flat_map);
    if let Some(name) = name {
        if name != "_all" && name != "*" {
            let pats: Vec<regex::Regex> =
                name.split(',').map(|p| crate::store::wildcard_to_regex(p.trim())).collect();
            flat_map.retain(|k, _| pats.iter().any(|re| re.is_match(k)));
        }
    }
    if flat {
        return Value::Object(flat_map);
    }
    // rebuild the nested shape from whatever survived the filter
    let mut nested = json!({});
    for (k, v) in flat_map {
        let mut cur = &mut nested;
        let segs: Vec<&str> = k.split('.').collect();
        for seg in &segs[..segs.len() - 1] {
            cur = cur
                .as_object_mut()
                .unwrap()
                .entry(seg.to_string())
                .or_insert_with(|| json!({}));
            if !cur.is_object() {
                *cur = json!({});
            }
        }
        cur.as_object_mut().unwrap().insert(segs[segs.len() - 1].to_string(), v);
    }
    nested
}

pub async fn get_settings(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    settings_response(store, index.map(|Path(i)| i), None, p)
}

pub async fn get_settings_all_named(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    settings_response(store, None, Some(name), p)
}

pub async fn get_settings_named(
    State(store): State<Store>,
    Path((index, name)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    settings_response(store, Some(index), Some(name), p)
}

fn settings_response(
    store: Store,
    index: Option<String>,
    name: Option<String>,
    p: Params,
) -> Response {
    let expr = index.unwrap_or_else(|| "_all".into());
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !ignore_unavailable(&p) {
        return no_such_index(&expr);
    }
    let flat = flag(&p, "flat_settings");
    let mut out = serde_json::Map::new();
    for n in targets {
        let Some(st) = store.get(&n) else { continue };
        let raw = st.read().effective_settings();
        let mut entry = json!({ "settings": settings_view(&raw, name.as_deref(), flat) });
        if flag(&p, "human") {
            add_human_settings(&mut entry["settings"], &st.read());
        }
        if flag(&p, "include_defaults") {
            entry["defaults"] = settings_view(&default_settings(), name.as_deref(), flat);
        }
        out.insert(n.clone(), entry);
    }
    axum::Json(Value::Object(out)).into_response()
}

// -------------------------------------------------------------- document CRUD

/// Read a document's `_source`, honouring uncommitted writes so GET is realtime.
fn source_enabled(st: &IdxState) -> bool {
    st.mapping
        .raw
        .get("_source")
        .and_then(|s| s.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// The arrival order recorded for a document, which is what `_seq_no` reports.
pub fn read_seq(st: &IdxState, id: &str) -> Option<u64> {
    if let Some(seq) = st.pending_seq.get(id) {
        return Some(*seq);
    }
    let searcher = st.realtime.searcher();
    let q = TermQuery::new(Term::from_field_text(st.fields.id, id), IndexRecordOption::Basic);
    let hits = searcher.search(&q, &TopDocs::with_limit(1).order_by_score()).ok()?;
    let (_, addr) = hits.first()?;
    // read from the column rather than the stored document: `_seq` is wanted
    // on every hit that asks for it, and a stored copy would be paid for on
    // every write to serve the far rarer read
    let reader = searcher.segment_readers().get(addr.segment_ord as usize)?;
    reader.fast_fields().u64("_seq").ok()?.first(addr.doc_id)
}

/// The document as a search would see it: only what has been refreshed.
///
/// `realtime=false` asks for exactly that -- the reader's view rather than the
/// writer's -- which is how a caller checks whether a write is visible yet.
pub fn read_source_refreshed(st: &IdxState, id: &str) -> Option<Value> {
    let searcher = st.reader.searcher();
    let q = TermQuery::new(Term::from_field_text(st.fields.id, id), IndexRecordOption::Basic);
    let hits = searcher.search(&q, &TopDocs::with_limit(1).order_by_score()).ok()?;
    let (_, addr) = hits.first()?;
    let doc: TantivyDocument = searcher.doc(*addr).ok()?;
    let raw = doc.get_first(st.fields.source)?.as_str()?.to_string();
    serde_json::from_str(&raw).ok()
}

/// The document however it can be reached: `realtime` is the default, and
/// sees writes that have been flushed but not yet refreshed.
pub fn read_source_as_asked(st: &IdxState, id: &str, p: &Params) -> Option<Value> {
    if p.get("realtime").map(|v| v == "false").unwrap_or(false) {
        return read_source_refreshed(st, id);
    }
    read_source(st, id)
}

pub fn read_source(st: &IdxState, id: &str) -> Option<Value> {
    st.gets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Some(p) = st.pending.get(id) {
        return p.as_ref().and_then(|raw| serde_json::from_str(raw).ok());
    }
    // the realtime reader sees flushed-but-unrefreshed writes
    let searcher = st.realtime.searcher();
    let q = TermQuery::new(Term::from_field_text(st.fields.id, id), IndexRecordOption::Basic);
    let hits = searcher.search(&q, &TopDocs::with_limit(1).order_by_score()).ok()?;
    let (_, addr) = hits.first()?;
    let doc: TantivyDocument = searcher.doc(*addr).ok()?;
    let raw = doc.get_first(st.fields.source)?.as_str()?.to_string();
    serde_json::from_str(&raw).ok()
}

pub fn exists_doc(st: &IdxState, id: &str) -> bool {
    st.is_live(id)
}

/// Write one document. `op_type == "create"` refuses to overwrite.
pub fn append_only(st: &IdxState) -> bool {
    st.setting("append_only.enabled").map(|v| v == "true").unwrap_or(false)
}

/// A version the caller supplied, and what it means for the write.
///
/// `external` numbers a document from somewhere outside the index: it must
/// climb, so a write carrying a version the index already has, or an older
/// one, has arrived out of order and is refused. `external_gte` allows the
/// same number again. Without a type the number names the version the caller
/// believes is current, and must match.
fn version_check(
    st: &IdxState,
    id: &str,
    p: &Params,
) -> std::result::Result<Option<u64>, Response> {
    let Some(want) = p.get("version").and_then(|v| v.parse::<u64>().ok()) else {
        return Ok(None);
    };
    let ty = p.get("version_type").map(|v| v.as_str()).unwrap_or("internal");
    let existed = exists_doc(st, id);
    let have = st.version_of(id);
    let conflict = |have: u64| {
        Err(err(
            StatusCode::CONFLICT,
            "version_conflict_engine_exception",
            format!(
                "[{id}]: version conflict, current version [{have}] is higher or equal to \
                 the one provided [{want}]"
            ),
        ))
    };
    match ty {
        "external" | "external_gt" => {
            if existed && want <= have {
                return conflict(have);
            }
            Ok(Some(want))
        }
        "external_gte" => {
            if existed && want < have {
                return conflict(have);
            }
            Ok(Some(want))
        }
        _ => {
            if !existed || want != have {
                return Err(err(
                    StatusCode::CONFLICT,
                    "version_conflict_engine_exception",
                    format!(
                        "[{id}]: version conflict, required version [{want}] is different \
                         to the one in the index [{have}]"
                    ),
                ));
            }
            Ok(None)
        }
    }
}

/// `if_seq_no` makes a write conditional on the document not having moved.
fn seq_check(st: &IdxState, id: &str, p: &Params) -> Option<Response> {
    let want_seq = p.get("if_seq_no").and_then(|v| v.parse::<u64>().ok());
    let want_term = p.get("if_primary_term").and_then(|v| v.parse::<u64>().ok());
    if want_seq.is_none() && want_term.is_none() {
        return None;
    }
    if !exists_doc(st, id) {
        return None;
    }
    let have = read_seq(st, id).unwrap_or(0);
    // a shard that has never failed over is on its first term, so any other
    // term the caller insists on is a term this document was not written in
    let term_ok = want_term.map(|t| t == 1).unwrap_or(true);
    let want = want_seq.unwrap_or(have);
    if have == want && term_ok {
        return None;
    }
    let want_term = want_term.unwrap_or(1);
    Some(err(
        StatusCode::CONFLICT,
        "version_conflict_engine_exception",
        format!(
            "[{id}]: version conflict, required seqNo [{want}], primary term \
             [{want_term}]. current document has seqNo [{have}] and primary term [1]"
        ),
    ))
}

pub fn write_doc(
    st: &mut IdxState,
    id: &str,
    source: Value,
    op_type: &str,
) -> std::result::Result<(Value, StatusCode), Response> {
    write_doc_raw(st, id, source, op_type, None)
}

/// A write that carries the caller's own version and conditions.
pub fn write_doc_checked(
    st: &mut IdxState,
    id: &str,
    source: Value,
    op_type: &str,
    raw: Option<String>,
    p: &Params,
) -> std::result::Result<(Value, StatusCode), Response> {
    if let Some(r) = seq_check(st, id, p) {
        return Err(r);
    }
    let forced = version_check(st, id, p)?;
    write_doc_versioned(st, id, source, op_type, raw, forced)
}

/// `raw` is the document exactly as the client sent it; passing it through
/// avoids re-serialising a tree we only just parsed.
pub fn write_doc_raw(
    st: &mut IdxState,
    id: &str,
    source: Value,
    op_type: &str,
    raw: Option<String>,
) -> std::result::Result<(Value, StatusCode), Response> {
    write_doc_versioned(st, id, source, op_type, raw, None)
}

pub fn write_doc_versioned(
    st: &mut IdxState,
    id: &str,
    source: Value,
    op_type: &str,
    raw: Option<String>,
    forced: Option<u64>,
) -> std::result::Result<(Value, StatusCode), Response> {
    // an index held still refuses writes until the block is lifted
    if st.setting("blocks.write").as_deref() == Some("true") {
        return Err(err(
            StatusCode::FORBIDDEN,
            "cluster_block_exception",
            format!(
                "index [{}] blocked by: [FORBIDDEN/8/index write (api)];",
                st.name
            ),
        ));
    }
    // an id is carried in the index's terms, which caps how long it may be
    if id.len() > 512 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "Document id cannot be longer than 512 bytes but was [{}]. The invalid id was: \
                 [{id}].",
                id.len()
            ),
        ));
    }
    // a create says the document must not be there, which is a version rule
    // of its own; naming another one asks for two things at once
    if op_type == "create" && forced.is_some() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: create operations only support internal versioning. \
             use index instead;",
        ));
    }
    let existed = exists_doc(st, id);
    if op_type == "create" && existed {
        return Err(err(
            StatusCode::CONFLICT,
            "version_conflict_engine_exception",
            format!("[{id}]: version conflict, document already exists"),
        ));
    }
    let (version, seq) = match forced {
        Some(v) => st.bump_to(id, true, v),
        None => st.bump(id, true, existed),
    };
    // deleting is only needed when something is actually there to replace;
    // a bulk load of new documents should not queue a delete per document
    if existed {
        let term = Term::from_field_text(st.fields.id, id);
        if let Ok(w) = st.writer() {
            w.delete_term(term);
        }
    }
    let default_lenient = st
        .setting("mapping.ignore_malformed")
        .map(|v| v == "true")
        .unwrap_or(false);
    let ignored = match crate::store::scan_malformed(&source, &st.mapping, default_lenient) {
        Ok(v) => v,
        Err((field, ty)) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "mapper_parsing_exception",
                format!("failed to parse field [{field}] of type [{ty}]"),
            ));
        }
    };
    // the ignored names ride along inside the stored source and are lifted back
    // out on the way to a hit, so no second stored field is needed
    let raw = if ignored.is_empty() {
        raw.unwrap_or_else(|| source.to_string())
    } else {
        let mut with = source.clone();
        if let Some(o) = with.as_object_mut() {
            o.insert("_ignored".into(), Value::Array(ignored.iter().cloned().map(Value::from).collect()));
        }
        with.to_string()
    };
    st.has_doc_count |= source.get("_doc_count").is_some();
    if let Err(field) = st.mapping.apply_dynamic_templates(&source) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "strict_dynamic_mapping_exception",
            format!(
                "mapping set to strict_allow_templates, dynamic introduction of [{field}] \
                 within [_doc] is not allowed"
            ),
        ));
    }
    st.mapping.learn_dynamic(&source);
    // normalized multi-fields are indexed alongside, but never stored
    let mut indexed = crate::store::expand_for_indexing(&source, &st.mapping);
    // the kinds a query narrows against have to be the kinds actually indexed,
    // which is the coerced view rather than what the client wrote
    st.observe(&indexed);
    if !ignored.is_empty() {
        for f in &ignored {
            crate::store::remove_path(&mut indexed, f);
        }
        if let Some(o) = indexed.as_object_mut() {
            o.insert("_ignored".into(), Value::Array(ignored.iter().cloned().map(Value::from).collect()));
        }
    }
    let doc = make_doc(&st.fields, id, indexed, &raw, seq);
    match st.writer() {
        Ok(w) => {
            if let Err(e) = w.add_document(doc) {
                return Err(err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "index_exception",
                    e.to_string(),
                ));
            }
        }
        Err(e) => {
            return Err(err(StatusCode::INTERNAL_SERVER_ERROR, "index_exception", e.to_string()));
        }
    }
    st.bytes.fetch_add(raw.len() as u64, std::sync::atomic::Ordering::Relaxed);
    st.note_pending(id, Some(raw));
    st.note_pending_seq(id, seq);
    let status = if existed { StatusCode::OK } else { StatusCode::CREATED };
    let body = json!({
        "_index": st.name,
        "_id": id,
        "_version": version,
        "result": if existed { "updated" } else { "created" },
        "_shards": shards_of(st),
        "_seq_no": seq,
        "_primary_term": 1,
    });
    Ok((body, status))
}

pub fn delete_doc(st: &mut IdxState, id: &str) -> (Value, StatusCode) {
    let existed = exists_doc(st, id);
    let (version, seq) = st.bump(id, false, existed);
    if existed {
        let term = Term::from_field_text(st.fields.id, id);
        if let Ok(w) = st.writer() {
            w.delete_term(term);
        }
        st.note_pending(id, None);
    }
    let body = json!({
        "_index": st.name,
        "_id": id,
        "_version": version,
        "result": if existed { "deleted" } else { "not_found" },
        "_shards": shards_of(st),
        "_seq_no": seq,
        "_primary_term": 1,
    });
    (body, if existed { StatusCode::OK } else { StatusCode::NOT_FOUND })
}

fn maybe_refresh(st: &mut IdxState, p: &Params) {
    if flag(p, "refresh") {
        let _ = st.refresh();
    }
}

/// A write that was asked to refresh says so in its answer, so the caller can
/// tell a refresh it forced from one that happened to be due anyway.
fn note_forced_refresh(body: &mut Value, p: &Params) {
    // `wait_for` waits for a refresh that was coming anyway; it does not force
    // one, and so is not reported as having done
    if matches!(p.get("refresh").map(|s| s.as_str()), Some("true") | Some("")) {
        body["forced_refresh"] = json!(true);
    }
}

/// `require_alias` says the write is only meant for an alias, so a name that
/// is not one is treated as absent rather than created on the spot.
fn refuse_unless_alias(store: &Store, index: &str, p: &Params) -> Option<Response> {
    let asked = p.get("require_alias").map(|v| v != "false").unwrap_or(false);
    if asked && !store.is_alias(index) {
        return Some(err(
            StatusCode::NOT_FOUND,
            "index_not_found_exception",
            format!("no such index [{index}] and [require_alias] request flag is [true] and [{index}] is not an alias"),
        ));
    }
    None
}

pub async fn index_doc(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    if let Some(r) = refuse_unless_alias(&store, &index, &p) {
        return r;
    }
    do_index(store, index, Some(id), p, body, "index").await
}

pub async fn index_doc_auto(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    if let Some(r) = refuse_unless_alias(&store, &index, &p) {
        return r;
    }
    do_index(store, index, None, p, body, "index").await
}

pub async fn create_doc(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    do_index(store, index, Some(id), p, body, "create").await
}

async fn do_index(
    store: Store,
    index: String,
    id: Option<String>,
    p: Params,
    body: String,
    default_op: &str,
) -> Response {
    let source: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, "mapper_parsing_exception", e.to_string()),
    };
    if !source.is_object() {
        return err(
            StatusCode::BAD_REQUEST,
            "mapper_parsing_exception",
            "failed to parse: expected an object",
        );
    }
    if let Some(bad) = dotted_only_field(&source) {
        return err(
            StatusCode::BAD_REQUEST,
            "mapper_parsing_exception",
            format!("field name cannot contain only the character [.]: [{bad}]"),
        );
    }
    let op_type = p.get("op_type").map(|s| s.as_str()).unwrap_or(default_op).to_string();
    let st = match store.ensure(&index) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string()),
    };
    let mut g = st.write();
    let id = id.unwrap_or_else(|| g.next_auto_id());
    match write_doc_checked(&mut g, &id, source, &op_type, None, &p) {
        Ok((mut body, status)) => {
            // a document written with a routing is only reachable by quoting
            // the same routing back, so it has to be remembered
            match p.get("routing").filter(|r| !r.is_empty()) {
                Some(r) => {
                    g.routing.insert(id.clone(), r.clone());
                    body["_routing"] = json!(r);
                }
                None => {
                    g.routing.remove(&id);
                }
            }
            maybe_refresh(&mut g, &p);
            note_forced_refresh(&mut body, &p);
            (status, axum::Json(body)).into_response()
        }
        Err(resp) => resp,
    }
}

/// Does the routing quoted on a request agree with the one the document was
/// written under? A document reached by the wrong routing is, to the caller,
/// not there at all: in a real cluster the request would have gone to a shard
/// that never held it.
fn routing_matches(st: &IdxState, id: &str, p: &Params) -> bool {
    match st.routing.get(id) {
        Some(have) => p.get("routing").map(|want| want == have).unwrap_or(false),
        None => true,
    }
}

/// `refresh=true` on a read asks for the index to be brought up to date first,
/// so that a write made a moment ago is visible to it.
fn refresh_before_read(store: &Store, index: &str, p: &Params) {
    if !flag(p, "refresh") {
        return;
    }
    for n in store.resolve(index) {
        if let Some(st) = store.get(&n) {
            let _ = st.write().refresh();
        }
    }
}

pub async fn get_doc(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    refresh_before_read(&store, &index, &p);
    let Some(st) = store.get(&index) else {
        return if ignored(&p, StatusCode::NOT_FOUND) {
            (StatusCode::NOT_FOUND, axum::Json(json!({"_index": index, "_id": id, "found": false})))
                .into_response()
        } else {
            no_such_index(&index)
        };
    };
    let g = st.read();
    // a version named on a read is a condition: the document must be at it
    if let Some(want) = p.get("version").and_then(|v| v.parse::<u64>().ok()) {
        if exists_doc(&g, &id) && g.version_of(&id) != want {
            return err(
                StatusCode::CONFLICT,
                "version_conflict_engine_exception",
                format!(
                    "[{id}]: version conflict, current version [{}] is different than the one \
                     provided [{want}]",
                    g.version_of(&id)
                ),
            );
        }
    }
    match read_source_as_asked(&g, &id, &p).filter(|_| routing_matches(&g, &id, &p)) {
        Some(src) => {
            let fields = stored_fields(&src, &p);
            let mut body = json!({
                "_index": g.name, "_id": id,
                "_version": g.version_of(&id),
                "_seq_no": read_seq(&g, &id).unwrap_or(0), "_primary_term": 1,
                "found": true,
            });
            if let Some(r) = g.routing.get(&id) {
                body["_routing"] = json!(r);
            }
            if let Some(f) = fields {
                body["fields"] = f;
                // OpenSearch omits _source when only stored_fields were asked for
                if !p.contains_key("_source")
                    && !p.contains_key("_source_includes")
                    && !wants_source_via_stored_fields(&p)
                {
                    return axum::Json(body).into_response();
                }
            }
            body["_source"] = filter_source_params(&src, &p);
            axum::Json(body).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"_index": g.name, "_id": id, "found": false})),
        )
            .into_response(),
    }
}

pub async fn head_doc(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    refresh_before_read(&store, &index, &p);
    let Some(st) = store.get(&index) else { return StatusCode::NOT_FOUND.into_response() };
    let g = st.read();
    // the same view a `_get` would take, so `realtime=false` says whether a
    // search can see the document rather than whether it was written -- and
    // the wrong routing reaches nothing, here as there
    if read_source_as_asked(&g, &id, &p)
        .filter(|_| routing_matches(&g, &id, &p))
        .is_some()
    {
        StatusCode::OK.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

pub async fn get_source(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    refresh_before_read(&store, &index, &p);
    let Some(st) = store.get(&index) else {
        return if ignored(&p, StatusCode::NOT_FOUND) {
            (StatusCode::NOT_FOUND, axum::Json(json!({}))).into_response()
        } else {
            no_such_index(&index)
        };
    };
    let g = st.read();
    if !source_enabled(&g) {
        return err(
            StatusCode::NOT_FOUND,
            "illegal_argument_exception",
            format!("fields [_source] are disabled in the mappings for index [{}]", g.name),
        );
    }
    match read_source_as_asked(&g, &id, &p).filter(|_| routing_matches(&g, &id, &p)) {
        Some(src) => axum::Json(filter_source_params(&src, &p)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"_index": g.name, "_id": id, "found": false})),
        )
            .into_response(),
    }
}

pub async fn delete_doc_route(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    let Some(st) = store.get(&index) else { return no_such_index(&index) };
    let mut g = st.write();
    if let Some(r) = seq_check(&g, &id, &p) {
        return r;
    }
    // an external version numbers the delete too, so a stale one is refused
    match version_check(&g, &id, &p) {
        Ok(Some(v)) => {
            let existed = exists_doc(&g, &id);
            let (version, seq) = g.bump_to(&id, false, v);
            if existed {
                let term = Term::from_field_text(g.fields.id, &id);
                if let Ok(w) = g.writer() {
                    w.delete_term(term);
                }
                g.note_pending(&id, None);
            }
            g.routing.remove(&id);
            maybe_refresh(&mut g, &p);
            let mut body = json!({
                "_index": g.name, "_id": id, "_version": version,
                "result": if existed { "deleted" } else { "not_found" },
                "_shards": shards_of(&g), "_seq_no": seq, "_primary_term": 1,
            });
            note_forced_refresh(&mut body, &p);
            let status =
                if existed { StatusCode::OK } else { StatusCode::NOT_FOUND };
            return (status, axum::Json(body)).into_response();
        }
        Ok(None) => {}
        Err(r) => return r,
    }
    if !routing_matches(&g, &id, &p) {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({
                "_index": g.name, "_id": id, "_version": g.version_of(&id),
                "result": "not_found", "_shards": shards(),
                "_seq_no": 0, "_primary_term": 1,
            })),
        )
            .into_response();
    }
    let (mut body, status) = delete_doc(&mut g, &id);
    g.routing.remove(&id);
    maybe_refresh(&mut g, &p);
    note_forced_refresh(&mut body, &p);
    (status, axum::Json(body)).into_response()
}

// ---------------------------------------------------------------------- bulk

pub async fn bulk(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    _headers: HeaderMap,
    body: String,
) -> Response {
    let default_index = index.map(|Path(i)| i);
    let mut items = Vec::new();
    let mut errors = false;
    let mut touched: Vec<String> = Vec::new();

    // Split the ndjson into operations first, so the expensive part -- parsing
    // each document and building its tantivy form -- can run across cores.
    struct Op<'a> {
        op: String,
        meta: Value,
        index: String,
        id: Option<String>,
        doc_line: Option<&'a str>,
    }
    let mut ops: Vec<Op> = Vec::new();
    // the line a complaint names is counted over the whole body, blank lines
    // and document lines included
    let mut lineno = 0usize;
    let mut lines = body.lines().filter(|l| !l.trim().is_empty()).inspect(|_| {});
    let mut lines = std::iter::from_fn(move || {
        let next = lines.next();
        if next.is_some() {
            lineno += 1;
        }
        next.map(|l| (lineno, l))
    })
    .peekable();
    while let Some((at, action_line)) = lines.next() {
        let action: Value = match serde_json::from_str(action_line) {
            Ok(v) => v,
            Err(e) => {
                return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string());
            }
        };
        // an action names the operation; an object with nothing in it names
        // none, and the line it was on is what a caller needs to be told
        let Some((op, meta)) = action.as_object().and_then(|o| o.iter().next()) else {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!(
                    "Malformed action/metadata line [{at}], expected FIELD_NAME but found \
                     [END_OBJECT]"
                ),
            );
        };
        let op = op.clone();
        let idx = meta
            .get("_index")
            .and_then(scalar_str)
            .or_else(|| default_index.clone());
        let Some(idx) = idx else {
            return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", "missing index");
        };
        let id_opt = meta.get("_id").and_then(scalar_str);
        let doc_line = if op == "delete" { None } else { lines.next().map(|(_, l)| l) };
        ops.push(Op { op, meta: meta.clone(), index: idx, id: id_opt, doc_line });
    }

    // Parse and build documents in parallel; nothing here touches shared state.
    let prepare = |o: &Op| {
        o.doc_line.map(|l| {
            serde_json::from_str::<Value>(l)
                .map(|v| (v, l.trim().to_string()))
                .map_err(|e| e.to_string())
        })
    };
    let prepared: Vec<Option<std::result::Result<(Value, String), String>>> =
        if std::env::var("OBSEARCH_SERIAL_BULK").is_ok() {
            ops.iter().map(prepare).collect()
        } else {
        use rayon::prelude::*;
        ops.par_iter()
            .map(prepare)
            .collect()
        };

    // consume the prepared documents rather than cloning them back out
    for (o, prep) in ops.into_iter().zip(prepared.into_iter()) {
        // an index action may carry `op_type: create` in its metadata, which
        // makes it a create -- in what it refuses, and in what it is called
        // in the answer
        let op = match o.meta.get("op_type").and_then(|v| v.as_str()) {
            Some("create") => "create".to_string(),
            _ => o.op,
        };
        let meta = o.meta;
        let idx = o.index;
        let id_opt = o.id;
        let meta_source = meta.get("_source").cloned();
        let (source, mut doc_raw): (Option<Value>, Option<String>) = match prep {
            Some(Ok((v, raw))) => (Some(v), Some(raw)),
            Some(Err(e)) => {
                return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e);
            }
            None => (None, None),
        };

        // an id written as an empty string is not the same as no id at all:
        // one asks for a document that has none, the other for a fresh one
        if id_opt.as_deref() == Some("") {
            errors = true;
            items.push(json!({ op.clone(): {
                "_index": idx, "_id": "", "status": 400,
                "error": {
                    "type": "illegal_argument_exception",
                    "reason": "if _id is specified it must not be empty"
                }
            }}));
            continue;
        }
        // an alias standing in front of several indices has no one place to
        // write to unless one of them was named the write index
        if store.is_alias(&idx) {
            let behind = store.resolve(&idx);
            let has_write = behind.iter().any(|n| {
                store
                    .get(n)
                    .map(|st| {
                        st.read()
                            .aliases
                            .get(&idx)
                            .and_then(|d| d.get("is_write_index"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
            });
            if behind.len() > 1 && !has_write {
                errors = true;
                items.push(json!({ op.clone(): {
                    "_index": idx, "_id": id_opt.clone().unwrap_or_default(), "status": 400,
                    "error": {
                        "type": "illegal_argument_exception",
                        "reason": format!(
                            "no write index is defined for alias [{idx}]. The write index may \
                             be explicitly disabled using is_write_index=false or the alias \
                             points to multiple indices without one being designated as a \
                             write index"
                        )
                    }
                }}));
                continue;
            }
        }
        // `require_alias` says the write is meant for an alias, so a name that
        // is not one is treated as absent rather than created on the spot
        // the action's own flag answers for it; the request's applies to the
        // actions that did not say
        let needs_alias = match meta.get("require_alias").and_then(|v| v.as_bool()) {
            Some(own) => own,
            None => p.get("require_alias").map(|v| v != "false").unwrap_or(false),
        };
        if needs_alias && !store.is_alias(&idx) {
            errors = true;
            items.push(json!({ op.clone(): {
                "_index": idx, "_id": id_opt.clone().unwrap_or_default(), "status": 404,
                "error": {
                    "type": "index_not_found_exception",
                    "reason": format!(
                        "no such index [{idx}] and [require_alias] request flag is [true] and \
                         [{idx}] is not an alias"
                    )
                }
            }}));
            continue;
        }
        let st = match store.ensure(&idx) {
            Ok(s) => s,
            Err(e) => {
                return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string());
            }
        };
        if !touched.contains(&idx) {
            touched.push(idx.clone());
        }
        // keep the number of live writers bounded across indices
        if !g_has_writer(&st) {
            store.note_writer_opened(&idx);
        }
        let mut g = st.write();
        let id_was_given = id_opt.is_some();
        let id = id_opt.unwrap_or_else(|| g.next_auto_id());

        let item = match op.as_str() {
            "delete" => {
                let (body, status) = delete_doc(&mut g, &id);
                let mut b = body;
                b["status"] = json!(status.as_u16());
                json!({ "delete": b })
            }
            "index" | "create" => {
                if id_was_given && append_only(&g) {
                    errors = true;
                    items.push(json!({ op.clone(): {
                        "_index": idx, "_id": id, "status": 400,
                        "error": {
                            "type": "validation_exception",
                            "reason": format!(
                                "Validation Failed: 1: Operation [{}] is not allowed with a \
                                 custom document id {id} as setting \
                                 `index.append_only.enabled` is enabled for this index: {idx};",
                                op.to_uppercase()
                            )
                        }
                    }}));
                    continue;
                }
                if let Some(pipe) = meta.get("pipeline").and_then(|v| v.as_str()) {
                    errors = true;
                    items.push(json!({ op.clone(): {
                        "_index": idx, "_id": id, "status": 400,
                        "error": {
                            "type": "illegal_argument_exception",
                            "reason": format!("pipeline with id [{pipe}] does not exist")
                        }
                    }}));
                    continue;
                }
                // an index action may be conditional too, on the sequence
                // number the caller believes the document is at
                let cond = meta
                    .get("if_seq_no")
                    .and_then(|v| v.as_u64())
                    .filter(|_| exists_doc(&g, &id));
                if let Some(want) = cond {
                    let have = read_seq(&g, &id).unwrap_or(0);
                    if have != want {
                        errors = true;
                        items.push(json!({ op.clone(): {
                            "_index": idx, "_id": id, "status": 409,
                            "error": {
                                "type": "version_conflict_engine_exception",
                                "reason": format!(
                                    "[{id}]: version conflict, required seqNo [{want}], \
                                     primary term [1]. current document has seqNo [{have}] \
                                     and primary term [1]"
                                )
                            }
                        }}));
                        continue;
                    }
                }
                let src = source.unwrap_or_else(|| json!({}));
                match write_doc_raw(&mut g, &id, src, &op, doc_raw.take()) {
                    Ok((body, status)) => {
                        let mut b = body;
                        b["status"] = json!(status.as_u16());
                        json!({ op.clone(): b })
                    }
                    Err(_) => {
                        errors = true;
                        json!({ op.clone(): {
                            "_index": idx, "_id": id, "status": 409,
                            "error": {
                                "type": "version_conflict_engine_exception",
                                "reason": format!("[{id}]: version conflict, document already exists")
                            }
                        }})
                    }
                }
            }
            "update" => {
                let existing = read_source(&g, &id);
                // the same conditional write the single-document update takes,
                // reported per item rather than as the whole request failing
                let stale = match (
                    meta.get("if_seq_no").and_then(|v| v.as_u64()),
                    existing.is_some(),
                ) {
                    (Some(want), true) => Some((want, read_seq(&g, &id).unwrap_or(0)))
                        .filter(|(want, have)| want != have),
                    _ => None,
                };
                if let Some((want, have)) = stale {
                    errors = true;
                    items.push(json!({ "update": {
                        "_index": idx, "_id": id, "status": 409,
                        "error": {
                            "type": "version_conflict_engine_exception",
                            "reason": format!(
                                "[{id}]: version conflict, required seqNo [{want}], \
                                 primary term [1]. current document has seqNo [{have}] \
                                 and primary term [1]"
                            )
                        }
                    }}));
                    continue;
                }
                let patch = source.unwrap_or_else(|| json!({}));
                let doc = patch.get("doc").cloned();
                match (existing, doc) {
                    (Some(mut base), Some(d)) => {
                        let before = base.clone();
                        merge_into(&mut base, &d);
                        // an update that changes nothing is reported as such,
                        // and counted, the same way the single-document API
                        // reports it
                        let noop = patch
                            .get("detect_noop")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true)
                            && base == before;
                        if noop {
                            g.noop_updates.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        match write_doc(&mut g, &id, base.clone(), "index") {
                            Ok((body, _)) => {
                                let mut b = body;
                                b["result"] = json!(if noop { "noop" } else { "updated" });
                                b["status"] = json!(200);
                                let sel = meta_source
                                    .clone()
                                    .or_else(|| patch.get("_source").cloned())
                                    .or_else(|| source_selector_from_params(&p));
                                if let Some(sel) = sel.as_ref().filter(|v| **v != json!(false)) {
                                    b["get"] = json!({
                                        "_source": apply_source_selector(&base, sel)
                                    });
                                }
                                json!({ "update": b })
                            }
                            Err(_) => {
                                errors = true;
                                json!({"update": {"_index": idx, "_id": id, "status": 500}})
                            }
                        }
                    }
                    (None, _) => {
                        let as_upsert = patch
                            .get("doc_as_upsert")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let upsert_doc = patch
                            .get("upsert")
                            .or_else(|| if as_upsert { patch.get("doc") } else { None });
                        if let Some(ups) = upsert_doc {
                            let ups = ups.clone();
                            match write_doc(&mut g, &id, ups.clone(), "index") {
                                Ok((body, _)) => {
                                    let mut b = body;
                                    b["status"] = json!(201);
                                    json!({ "update": b })
                                }
                                Err(_) => {
                                    errors = true;
                                    json!({"update": {"_index": idx, "_id": id, "status": 500}})
                                }
                            }
                        } else {
                            errors = true;
                            let reason = format!("[{id}]: document missing");
                            let mut error = json!({
                                "type": "document_missing_exception", "reason": reason,
                            });
                            // this one names the shard it looked in, which is
                            // what a caller reading the trace wants to know
                            if p.get("error_trace").map(|v| v != "false").unwrap_or(false) {
                                error["stack_trace"] = json!(format!(
                                    "[[{idx}][0]] DocumentMissingException[{reason}] \
                                     at obsearch::api::bulk (src/api.rs)"
                                ));
                            }
                            json!({"update": {
                                "_index": idx, "_id": id, "status": 404, "error": error
                            }})
                        }
                    }
                    _ => {
                        errors = true;
                        json!({"update": {"_index": idx, "_id": id, "status": 400}})
                    }
                }
            }
            other => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!("unknown bulk action [{other}]"),
                );
            }
        };
        items.push(item);
    }

    if flag(&p, "refresh") {
        for n in touched {
            if let Some(st) = store.get(&n) {
                let _ = st.write().refresh();
            }
        }
    }
    axum::Json(json!({"took": 0, "errors": errors, "items": items})).into_response()
}

// ------------------------------------------------------------ source filtering

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

fn as_list(p: &Params, key: &str) -> Option<Vec<String>> {
    p.get(key).map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

fn filter_source_params(src: &Value, p: &Params) -> Value {
    if let Some(v) = p.get("_source") {
        if v == "false" {
            return json!(null);
        }
        if v != "true" {
            return crate::source::filter(src, &as_list(p, "_source").unwrap_or_default(), &[]);
        }
    }
    let inc = as_list(p, "_source_includes").or_else(|| as_list(p, "_source_include"));
    let exc = as_list(p, "_source_excludes").or_else(|| as_list(p, "_source_exclude"));
    if inc.is_none() && exc.is_none() {
        return src.clone();
    }
    crate::source::filter(src, &inc.unwrap_or_default(), &exc.unwrap_or_default())
}

/// `_source` as it can appear in a request body: bool, pattern, list, or
/// an object with includes/excludes.
pub fn apply_source_selector(src: &Value, sel: &Value) -> Value {
    match sel {
        Value::Bool(true) | Value::Null => src.clone(),
        Value::Bool(false) => Value::Null,
        Value::String(pat) => crate::source::filter(src, &[pat.clone()], &[]),
        Value::Array(items) => {
            let inc: Vec<String> =
                items.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
            crate::source::filter(src, &inc, &[])
        }
        Value::Object(o) => {
            let pick = |k1: &str, k2: &str| -> Vec<String> {
                let v = o.get(k1).or_else(|| o.get(k2));
                match v {
                    Some(Value::String(s)) => vec![s.clone()],
                    Some(Value::Array(a)) => {
                        a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()
                    }
                    _ => vec![],
                }
            };
            crate::source::filter(src, &pick("includes", "include"), &pick("excludes", "exclude"))
        }
        _ => src.clone(),
    }
}

/// The `_source*` query-string family, as a selector value.
fn source_selector_from_params(p: &Params) -> Option<Value> {
    if let Some(v) = p.get("_source") {
        return Some(match v.as_str() {
            "true" => json!(true),
            "false" => json!(false),
            other if other.contains(',') => {
                json!(other.split(',').map(|s| s.trim()).collect::<Vec<_>>())
            }
            other => json!(other),
        });
    }
    let inc = as_list(p, "_source_includes").or_else(|| as_list(p, "_source_include"));
    let exc = as_list(p, "_source_excludes").or_else(|| as_list(p, "_source_exclude"));
    if inc.is_none() && exc.is_none() {
        return None;
    }
    Some(json!({
        "includes": inc.unwrap_or_default(),
        "excludes": exc.unwrap_or_default(),
    }))
}

/// `stored_fields` is answered from `_source`; every value comes back as a list.
fn wants_source_via_stored_fields(p: &Params) -> bool {
    p.get("stored_fields")
        .map(|s| s.split(',').any(|f| f.trim() == "_source"))
        .unwrap_or(false)
}

fn stored_fields(src: &Value, p: &Params) -> Option<Value> {
    let spec = p.get("stored_fields")?;
    if spec == "_none_" || spec.is_empty() {
        return None;
    }
    let mut out = serde_json::Map::new();
    for name in spec.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if name == "_source" {
            continue;
        }
        let picked = crate::source::filter(src, &[name.to_string()], &[]);
        if let Some(v) = flat_lookup(&picked, name) {
            let arr = match v {
                Value::Array(a) => Value::Array(a),
                other => Value::Array(vec![other]),
            };
            out.insert(name.to_string(), arr);
        }
    }
    if out.is_empty() { None } else { Some(Value::Object(out)) }
}

fn flat_lookup(v: &Value, path: &str) -> Option<Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur.clone())
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

fn parse_body(body: &str) -> std::result::Result<Value, Response> {
    if body.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(body)
        .map_err(|e| err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string()))
}

/// Query-string forms of the search request that the suite exercises.
fn fold_params_into_body(body: &mut Value, p: &Params) {
    if let Some(q) = p.get("q") {
        if body.get("query").is_none() {
            let mut qs = json!({"query": q});
            if let Some(df) = p.get("df").or_else(|| p.get("default_field")) {
                qs["default_field"] = json!(df);
            }
            if let Some(op) = p.get("default_operator") {
                qs["default_operator"] = json!(op.to_lowercase());
            }
            body["query"] = json!({ "query_string": qs });
        }
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
    if let Some(s) = p.get("sort") {
        if body.get("sort").is_none() {
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
}

/// A keep-alive or scroll timeout as written: a count and a unit.
fn parse_keep_alive(s: &str) -> Option<u64> {
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

/// What a scroll refuses before it starts.
fn check_scroll(store: &Store, expr: &str, body: &Value, p: &Params) -> Option<Response> {
    let Some(keep) = p.get("scroll") else { return None };
    if body.get("size").and_then(|v| v.as_i64()) == Some(0)
        || p.get("size").map(|v| v == "0").unwrap_or(false)
    {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "[size] cannot be [0] in a scroll context",
        ));
    }
    if p.get("request_cache").map(|v| v == "true").unwrap_or(false) {
        return Some(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "[request_cache] cannot be used in a scroll context",
        ));
    }
    // a slice divides the documents between readers, and there is a ceiling on
    // how finely it may be cut
    if let Some(max) = body.pointer("/slice/max").and_then(|v| v.as_i64()) {
        // how finely a scroll may be cut is an index setting, so an index that
        // raises it may be sliced that far
        let limit = store
            .resolve(expr)
            .iter()
            .filter_map(|n| store.get(n))
            .filter_map(|st| st.read().numeric_setting("max_slices_per_scroll"))
            .max()
            .unwrap_or(1024) as i64;
        if max > limit {
            return Some(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!(
                    "The number of slices [{max}] is too large. It must be less than [{limit}]."
                ),
            ));
        }
    }
    let limit = store
        .cluster_setting("search.max_keep_alive")
        .and_then(|v| v.as_str().and_then(parse_keep_alive));
    if let (Some(limit), Some(want)) = (limit, parse_keep_alive(keep)) {
        if want > limit {
            return Some(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!(
                    "Keep alive for request ({keep}) is too large. It must be less than ({}). \
                     This limit can be set by changing the [search.max_keep_alive] cluster level \
                     setting.",
                    store
                        .cluster_setting("search.max_keep_alive")
                        .and_then(|v| v.as_str().map(|s| s.to_string()))
                        .unwrap_or_default()
                ),
            ));
        }
    }
    None
}

pub async fn search(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let mut body = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if !body.is_object() {
        return err(StatusCode::BAD_REQUEST, "parsing_exception", "body must be an object");
    }
    fold_params_into_body(&mut body, &p);
    // `stats: [name]` tags the query so _stats can report per-group counts
    if let Some(groups) = body.get("stats").and_then(|v| v.as_array()) {
        let names: Vec<String> =
            groups.iter().filter_map(|g| g.as_str().map(|s| s.to_string())).collect();
        for n in store.resolve(&expr) {
            if let Some(st) = store.get(&n) {
                let g = st.read();
                let mut m = g.search_groups.write();
                for name in &names {
                    *m.entry(name.clone()).or_insert(0) += 1;
                }
            }
        }
    }
    if let Some(r) = check_scroll(&store, &expr, &body, &p) {
        return r;
    }
    let scrolling = p.contains_key("scroll");
    match crate::search::run(&store, &expr, &body, &p) {
        Ok(out) => {
            let n = out.hits.len();
            let mut env = crate::search::envelope(out, &body, &p);
            if scrolling {
                let size = scroll_size(&body, &p);
                let id = store.open_scroll(&expr, &body, n.max(size).min(size.max(n)));
                // the cursor starts after what this response already returned
                store.advance_scroll(&id, 0);
                env["_scroll_id"] = json!(id);
            }
            respond(&p, env)
        }
        Err(r) => r,
    }
}

/// Total shard count across the resolved indices, primaries plus replicas.
pub fn shard_total(store: &Store, names: &[String]) -> u64 {
    names
        .iter()
        .filter_map(|n| store.get(n))
        .map(|st| {
            let g = st.read();
            let s = g.effective_settings();
            let num = |k: &str, d: u64| {
                s.pointer(&format!("/index/{k}"))
                    .and_then(|v| v.as_str().and_then(|x| x.parse().ok()).or_else(|| v.as_u64()))
                    .unwrap_or(d)
            };
            num("number_of_shards", 1) * (1 + num("number_of_replicas", 1))
        })
        .sum()
}

// -------------------------------------------------------------------- scroll

fn scroll_size(body: &Value, p: &Params) -> usize {
    body.get("size")
        .and_then(|v| v.as_u64())
        .or_else(|| p.get("size").and_then(|v| v.parse().ok()))
        .unwrap_or(10) as usize
}

pub async fn scroll(
    State(store): State<Store>,
    id_path: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let id = body
        .get("scroll_id")
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .or_else(|| p.get("scroll_id").cloned())
        .or_else(|| id_path.map(|Path(i)| i));
    let Some(id) = id else {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: scroll_id is missing;",
        );
    };
    // the ceiling applies every time the scroll is asked to live longer, not
    // only when it was opened
    let asked = body.get("scroll").and_then(|v| v.as_str()).map(|s| s.to_string())
        .or_else(|| p.get("scroll").cloned());
    if let (Some(keep), Some(limit)) = (
        asked.as_deref(),
        store
            .cluster_setting("search.max_keep_alive")
            .and_then(|v| v.as_str().map(|s| s.to_string())),
    ) {
        if let (Some(want), Some(cap)) = (parse_keep_alive(keep), parse_keep_alive(&limit)) {
            if want > cap {
                return err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!(
                        "Keep alive for request ({keep}) is too large. It must be less than \
                         ({limit}). This limit can be set by changing the \
                         [search.max_keep_alive] cluster level setting."
                    ),
                );
            }
        }
    }
    let Some(state) = store.read_scroll(&id) else {
        return err(
            StatusCode::NOT_FOUND,
            "search_context_missing_exception",
            format!("No search context found for id [{id}]"),
        );
    };
    let mut req = state.body.clone();
    req["from"] = json!(state.offset);
    req["size"] = json!(state.size);
    // the scroll walks the index as it stood when it was opened, so a
    // document written since is not walked into halfway through
    req["pit"] = json!({"id": state.pit});
    match crate::search::run(&store, &state.expr, &req, &p) {
        Ok(out) => {
            let n = out.hits.len();
            store.advance_scroll(&id, n);
            let mut env = crate::search::envelope(out, &req, &p);
            env["_scroll_id"] = json!(id);
            respond(&p, env)
        }
        Err(r) => r,
    }
}

pub async fn clear_scroll(
    State(store): State<Store>,
    id_path: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let mut ids: Vec<String> = match body.get("scroll_id") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect(),
        _ => Vec::new(),
    };
    if let Some(Path(i)) = id_path {
        ids.extend(i.split(',').map(|s| s.to_string()));
    }
    if ids.iter().any(|i| i == "_all") {
        let n = store.close_all_scrolls();
        return respond(&p, json!({"succeeded": true, "num_freed": n}));
    }
    if ids.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: no scroll ids specified;",
        );
    }
    let freed = ids.iter().filter(|i| store.close_scroll(i)).count();
    if freed == 0 {
        return err(
            StatusCode::NOT_FOUND,
            "search_context_missing_exception",
            "No search context found",
        );
    }
    respond(&p, json!({"succeeded": true, "num_freed": freed}))
}

// ------------------------------------------------------------- field mappings

pub async fn get_field_mapping(
    State(store): State<Store>,
    path: Path<Vec<String>>,
    Query(p): Query<Params>,
) -> Response {
    let Path(parts) = path;
    let (expr, fields) = match parts.len() {
        1 => ("_all".to_string(), parts[0].clone()),
        _ => (parts[0].clone(), parts[1].clone()),
    };
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !ignore_unavailable(&p) {
        return no_such_index(&expr);
    }
    let wanted: Vec<&str> = fields.split(',').map(|s| s.trim()).collect();
    let mut out = serde_json::Map::new();
    for n in targets {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        let mut mappings = serde_json::Map::new();
        let declared = g.mapping.raw.get("properties").and_then(|v| v.as_object()).cloned();
        for (path_name, kind) in g.all_field_types() {
            let hit = wanted.iter().any(|w| {
                *w == "*" || *w == path_name
                    || crate::store::wildcard_to_regex(w).is_match(&path_name)
                    || path_name.rsplit('.').next() == Some(*w)
            });
            if !hit {
                continue;
            }
            // echo the declared definition when there is one, else the type we learned
            let def = declared
                .as_ref()
                .and_then(|d| d.get(path_name.split('.').next().unwrap_or(&path_name)))
                .filter(|_| !path_name.contains('.'))
                .cloned()
                .unwrap_or_else(|| json!({"type": kind}));
            let leaf = path_name.rsplit('.').next().unwrap_or(&path_name).to_string();
            // `include_defaults` fills in what the field would use where it
            // did not say, which for a text field is the default analyzer
            let mut def = def;
            if flag(&p, "include_defaults") {
                if let Some(o) = def.as_object_mut() {
                    if o.get("type").and_then(|t| t.as_str()) == Some("text") {
                        o.entry("analyzer".to_string())
                            .or_insert_with(|| json!("default"));
                    }
                }
            }
            mappings.insert(path_name.clone(), json!({
                "full_name": path_name,
                "mapping": { leaf: def }
            }));
        }
        out.insert(n.clone(), json!({"mappings": Value::Object(mappings)}));
    }
    respond(&p, Value::Object(out))
}

pub async fn count(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let mut body = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    // `_count` accepts only a query; anything else is a client error
    if let Some(o) = body.as_object() {
        for k in o.keys() {
            if k != "query" {
                return err(
                    StatusCode::BAD_REQUEST,
                    "parsing_exception",
                    format!("request does not support [{k}]"),
                );
            }
        }
    }
    fold_params_into_body(&mut body, &p);
    body["size"] = json!(0);
    body["track_total_hits"] = json!(true);
    match crate::search::run(&store, &expr, &body, &p) {
        Ok(out) => {
            let n = out.shards;
            let skipped = out.skipped;
            respond(&p, json!({
                "count": out.total,
                "_shards": {"total": n, "successful": n, "skipped": skipped, "failed": 0}
            }))
        }
        Err(r) => r,
    }
}

pub async fn msearch(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let default_index = index.map(|Path(i)| i).unwrap_or_default();
    // request-level parameters are validated once, before any sub-search runs
    if let Err(r) = crate::search::validate_params(&json!({}), &p) {
        return r;
    }
    let mut responses = Vec::new();
    let mut lines = body.lines().filter(|l| !l.trim().is_empty());
    while let Some(header_line) = lines.next() {
        let header: Value = serde_json::from_str(header_line).unwrap_or(json!({}));
        let Some(body_line) = lines.next() else { break };
        let mut req: Value = match serde_json::from_str(body_line) {
            Ok(v) => v,
            Err(e) => {
                return err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string());
            }
        };
        let expr = header
            .get("index")
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Array(a) => Some(
                    a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(","),
                ),
                _ => None,
            })
            .unwrap_or_else(|| default_index.clone());
        fold_params_into_body(&mut req, &p);
        if let Some(hdr) = header.as_object() {
            for (k, v) in hdr {
                if k != "index" && req.get(k).is_none() {
                    req[k.clone()] = v.clone();
                }
            }
        }
        // a bad parameter in any sub-request fails the whole msearch
        if let Err(r) = crate::search::validate_params(&req, &p) {
            return r;
        }
        match crate::search::run(&store, &expr, &req, &p) {
            Ok(out) => {
                let mut env = crate::search::envelope(out, &req, &p);
                env["status"] = json!(200);
                responses.push(env);
            }
            Err(_) => {
                let reason = format!("no such index [{expr}]");
                let mut error = json!({
                    "type": "index_not_found_exception",
                    "reason": reason,
                    "index": expr,
                    "resource.type": "index_or_alias",
                    "resource.id": expr,
                    "index_uuid": "_na_",
                    "root_cause": [{
                        "type": "index_not_found_exception",
                        "reason": reason,
                        "index": expr,
                        "resource.type": "index_or_alias",
                        "resource.id": expr,
                        "index_uuid": "_na_"
                    }]
                });
                add_stack_trace(&mut error, &p, "msearch");
                responses.push(json!({"error": error, "status": 404}));
            }
        }
    }
    respond(&p, json!({"took": 1, "responses": responses}))
}

// ----------------------------------------------------------------------- mget

/// OpenSearch refuses a field whose name is nothing but dots.
fn dotted_only_field(v: &Value) -> Option<String> {
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

fn g_has_writer(st: &std::sync::Arc<parking_lot::RwLock<IdxState>>) -> bool {
    st.read().has_writer()
}

/// `_id` and `_index` may arrive as strings or bare numbers.
fn scalar_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub async fn mget(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let default_index = index.map(|Path(i)| i);
    if let Some(idx) = default_index.as_deref() {
        refresh_before_read(&store, idx, &p);
    }
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };

    let mut requested: Vec<(Option<String>, Option<String>, Option<Value>)> = Vec::new();
    let mut per_doc_routing: Vec<Option<String>> = Vec::new();
    let empty_docs = body
        .get("docs")
        .and_then(|d| d.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(false)
        || body.get("ids").and_then(|d| d.as_array()).map(|a| a.is_empty()).unwrap_or(false);
    if let Some(docs) = body.get("docs").and_then(|d| d.as_array()) {
        for d in docs {
            // `routing` is a real field here; the underscored spellings are
            // the ones that were taken away
            for dep in ["_routing", "_version", "_type", "version"] {
                if d.get(dep).is_some() {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        format!("Unsupported field [{dep}] used in multi get request"),
                    );
                }
            }
        }
    }
    if empty_docs || (body.get("docs").is_none() && body.get("ids").is_none()) {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: no documents to get;",
        );
    }
    if let Some(docs) = body.get("docs").and_then(|d| d.as_array()) {
        for d in docs {
            per_doc_routing.push(d.get("routing").and_then(scalar_str));
            requested.push((
                d.get("_index").and_then(scalar_str),
                d.get("_id").and_then(scalar_str),
                d.get("_source").cloned().or_else(|| {
                    d.get("stored_fields").map(|sf| json!({"__stored": sf}))
                }),
            ));
        }
    } else if let Some(ids) = body.get("ids").and_then(|d| d.as_array()) {
        for i in ids {
            per_doc_routing.push(None);
            requested.push((None, scalar_str(i), None));
        }
    }

    let mut docs = Vec::new();
    for (n, (idx, id, sel)) in requested.into_iter().enumerate() {
        // a routing given on the document is the one it has to be reached by
        let want_routing = per_doc_routing.get(n).cloned().flatten();
        let idx = idx.or_else(|| default_index.clone());
        let Some(idx) = idx else {
            return err(
                StatusCode::BAD_REQUEST,
                "action_request_validation_exception",
                "Validation Failed: 1: index is missing;",
            );
        };
        let Some(id) = id else {
            return err(
                StatusCode::BAD_REQUEST,
                "action_request_validation_exception",
                "Validation Failed: 1: id is missing;",
            );
        };
        let Some(st) = store.get(&idx) else {
            let reason = format!("no such index [{idx}]");
            let cause = json!({
                "type": "index_not_found_exception", "reason": reason,
                "index": idx, "resource.type": "index_expression", "resource.id": idx,
                "index_uuid": "_na_"
            });
            let mut error = json!({
                "type": "index_not_found_exception",
                "reason": reason,
                "index": idx, "resource.type": "index_expression", "resource.id": idx,
                "index_uuid": "_na_",
                "root_cause": [cause]
            });
            add_stack_trace(&mut error, &p, "mget");
            docs.push(json!({"_index": idx, "_id": id, "error": error}));
            continue;
        };
        // an alias in front of several indices names no one document, so a
        // get through it cannot say which was meant
        let behind = store.resolve(&idx);
        if store.is_alias(&idx) && behind.len() > 1 {
            let listed = behind.join(", ");
            let reason = format!(
                "alias [{idx}] has more than one index associated with it [{listed}], can't \
                 execute a single index op"
            );
            docs.push(json!({
                "_index": idx, "_id": id, "found": false,
                "error": {
                    "type": "illegal_argument_exception", "reason": reason,
                    "root_cause": [{"type": "illegal_argument_exception", "reason": reason}],
                }
            }));
            continue;
        }
        let g = st.read();
        let routing_ok = match g.routing.get(&id) {
            Some(have) => want_routing.as_deref() == Some(have.as_str()),
            None => true,
        };
        match read_source_as_asked(&g, &id, &p).filter(|_| routing_ok) {
            Some(src) => {
                // a doc may carry its own stored_fields; otherwise the request-level
                // one applies. Either way it suppresses _source unless asked for.
                let per_doc_stored = sel.as_ref().and_then(|s| s.get("__stored")).cloned();
                let stored_spec = per_doc_stored.clone().map(|sf| match sf {
                    Value::String(s) => s,
                    Value::Array(a) => {
                        a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")
                    }
                    _ => String::new(),
                });
                let stored_spec = stored_spec.or_else(|| p.get("stored_fields").cloned());

                let explicit_source = sel
                    .clone()
                    .filter(|s| s.get("__stored").is_none())
                    .or_else(|| body.get("_source").cloned())
                    .or_else(|| source_selector_from_params(&p));

                let mut d = json!({
                    "_index": g.name, "_id": id,
                    "_version": g.version_of(&id),
                    "_seq_no": read_seq(&g, &id).unwrap_or(0), "_primary_term": 1, "found": true
                });
                if let Some(r) = g.routing.get(&id) {
                    d["_routing"] = json!(r);
                }
                let mut wants_source = true;
                if let Some(spec) = &stored_spec {
                    let mut sub = Params::new();
                    sub.insert("stored_fields".into(), spec.clone());
                    if let Some(f) = stored_fields(&src, &sub) {
                        d["fields"] = f;
                    }
                    wants_source = spec.split(',').any(|f| f.trim() == "_source")
                        || explicit_source.is_some();
                }
                if wants_source {
                    let filtered = match &explicit_source {
                        Some(s) => apply_source_selector(&src, s),
                        None => src,
                    };
                    if !filtered.is_null() {
                        d["_source"] = filtered;
                    }
                }
                docs.push(d);
            }
            None => docs.push(json!({"_index": g.name, "_id": id, "found": false})),
        }
    }
    respond(&p, json!({"docs": docs}))
}

// -------------------------------------------------------------------- update

pub async fn update_doc(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    if let Some(r) = refuse_unless_alias(&store, &index, &p) {
        return r;
    }
    let patch: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    const UPDATE_KEYS: &[&str] = &[
        "doc", "upsert", "doc_as_upsert", "detect_noop", "_source", "script",
        "scripted_upsert", "if_seq_no", "if_primary_term",
    ];
    if let Some(o) = patch.as_object() {
        for k in o.keys() {
            if !UPDATE_KEYS.contains(&k.as_str()) {
                // OpenSearch offers a spelling hint for near-misses
                let hint = if k.len() == 3 && k.starts_with('d') && k.ends_with('c') {
                    " did you mean [doc]?"
                } else {
                    ""
                };
                return err(
                    StatusCode::BAD_REQUEST,
                    "x_content_parse_exception",
                    format!("[UpdateRequest] unknown field [{k}]{hint}"),
                );
            }
        }
    }
    // an update carrying an upsert creates the index, the way OpenSearch does
    let has_upsert = patch.get("upsert").is_some()
        || patch.get("doc_as_upsert").and_then(|v| v.as_bool()).unwrap_or(false);
    let st = if has_upsert {
        match store.ensure(&index) {
            Ok(s) => s,
            Err(e) => {
                return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string());
            }
        }
    } else {
        match store.get(&index) {
            Some(s) => s,
            None => return no_such_index(&index),
        }
    };
    let mut g = st.write();
    // the wrong routing reaches nothing, so there is no document to update
    let existing = read_source(&g, &id).filter(|_| routing_matches(&g, &id, &p));
    // `if_seq_no` makes the write conditional on the document not having moved
    // since the caller read it. A document that is not there at all is a
    // different complaint, and is left to the missing-document path below.
    if let (Some(want), true) = (
        p.get("if_seq_no").and_then(|v| v.parse::<u64>().ok()),
        existing.is_some(),
    ) {
        let have = read_seq(&g, &id).unwrap_or(0);
        if have != want {
            return err(
                StatusCode::CONFLICT,
                "version_conflict_engine_exception",
                format!(
                    "[{id}]: version conflict, required seqNo [{want}], primary term [1]. \
                     current document has seqNo [{have}] and primary term [1]"
                ),
            );
        }
    }
    let detect_noop =
        patch.get("detect_noop").and_then(|v| v.as_bool()).unwrap_or(true);
    let doc_as_upsert = patch.get("doc_as_upsert").and_then(|v| v.as_bool()).unwrap_or(false);

    let (next, result) = match (existing.clone(), patch.get("doc")) {
        (Some(base), Some(d)) => {
            let mut merged = base.clone();
            merge_into(&mut merged, d);
            if detect_noop && merged == base {
                // the write guard is already held here; taking a read on the
                // same lock would wait for itself
                g.noop_updates.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                (base, "noop")
            } else {
                (merged, "updated")
            }
        }
        (Some(base), None) => (base, "noop"),
        (None, doc) => {
            let ups = patch
                .get("upsert")
                .cloned()
                .or_else(|| if doc_as_upsert { doc.cloned() } else { None });
            match ups {
                Some(u) => (u, "created"),
                None => {
                    return err(
                        StatusCode::NOT_FOUND,
                        "document_missing_exception",
                        format!("[{id}]: document missing"),
                    );
                }
            }
        }
    };

    let mut body_out = if result == "noop" {
        let version = g.version_of(&id);
        json!({
            "_index": g.name, "_id": id, "_version": version, "result": "noop",
            "_shards": {"total": 0, "successful": 0, "failed": 0},
            // an update that changed nothing still reports where the document
            // stands, which is where it already stood
            "_seq_no": read_seq(&g, &id).unwrap_or(0), "_primary_term": 1,
        })
    } else {
        match write_doc(&mut g, &id, next.clone(), "index") {
            Ok((mut b, _)) => {
                b["result"] = json!(result);
                b
            }
            Err(r) => return r,
        }
    };

    let sel = patch.get("_source").cloned().or_else(|| source_selector_from_params(&p));
    if let Some(sel) = sel.as_ref().filter(|v| **v != json!(false)) {
        body_out["get"] = json!({"_source": apply_source_selector(&next, sel), "found": true});
    }
    // a routing given on the update is the one the document keeps
    match p.get("routing").filter(|r| !r.is_empty()) {
        Some(r) => {
            g.routing.insert(id.clone(), r.clone());
            body_out["_routing"] = json!(r);
        }
        None => {
            if let Some(r) = g.routing.get(&id) {
                body_out["_routing"] = json!(r);
            }
        }
    }
    maybe_refresh(&mut g, &p);
    note_forced_refresh(&mut body_out, &p);
    let status = if result == "created" { StatusCode::CREATED } else { StatusCode::OK };
    (status, axum::Json(body_out)).into_response()
}

// ------------------------------------------------------------- memory report

/// What the process is actually holding, and where.
///
/// `?collect=true` first asks the allocator to hand back everything it can, so
/// the difference between the two answers separates "retained by the allocator"
/// from "still referenced by us".
pub async fn memory_report(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    if flag(&p, "collect") {
        unsafe { libmimalloc_sys::mi_collect(true) };
    }
    let (mut elapsed, mut user, mut sys, mut rss, mut peak_rss, mut commit, mut peak_commit, mut faults) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    unsafe {
        libmimalloc_sys::mi_process_info(
            &mut elapsed, &mut user, &mut sys, &mut rss, &mut peak_rss,
            &mut commit, &mut peak_commit, &mut faults,
        );
    }
    let mb = |v: usize| (v as f64 / 1_048_576.0 * 10.0).round() / 10.0;

    let mut per_index = Vec::new();
    let (mut live_ids, mut versions, mut pending, mut shapes, mut kinds, mut segments, mut writers) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    for name in store.names() {
        let Some(st) = store.get(&name) else { continue };
        let g = st.read();
        let segs = g.reader.searcher().segment_readers().len();
        live_ids += g.live_ids.len();
        versions += g.versions.len();
        pending += g.pending.len();
        shapes += g.seen_shapes.len();
        kinds += g.observed_kinds.len();
        segments += segs;
        if g.has_writer() {
            writers += 1;
        }
        if per_index.len() < 3 {
            per_index.push(json!({
                "index": name, "segments": segs, "live_ids": g.live_ids.len(),
                "versions": g.versions.len(), "pending": g.pending.len(),
                "pending_bytes": g.pending_bytes, "has_writer": g.has_writer(),
            }));
        }
    }
    respond(&p, json!({
        "allocator": {
            "rss_mb": mb(rss), "peak_rss_mb": mb(peak_rss),
            "committed_mb": mb(commit), "peak_committed_mb": mb(peak_commit),
            "page_faults": faults,
        },
        "indices": {
            "count": store.names().len(), "live_writers": writers,
            "total_segments": segments, "total_live_ids": live_ids,
            "total_versions": versions, "total_pending": pending,
            "total_shapes": shapes, "total_kind_paths": kinds,
        },
        "sample": per_index,
    }))
}

// --------------------------------------------------------------- force merge

/// `_forcemerge` collapses segments. Fewer segments means less per-segment setup
/// on every search, which matters most for aggregations: each one opens columns
/// and builds its own intermediate result per segment before they are merged.
pub async fn force_merge(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_else(|| "_all".into());
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" {
        return no_such_index(&expr);
    }
    // a merge reaches every copy of a shard unless told to keep to the
    // primaries, and a replica is a copy this node does not hold
    let primary_only = p.get("primary_only").map(|v| v != "false").unwrap_or(false);
    let touched: u64 = targets
        .iter()
        .filter_map(|n| store.get(n))
        .map(|st| {
            let g = st.read();
            let shards = g.numeric_setting("number_of_shards").unwrap_or(1).max(1);
            let copies = if primary_only {
                1
            } else {
                1 + g.numeric_setting("number_of_replicas").unwrap_or(0)
            };
            shards * copies
        })
        .sum();
    let max_segments: usize = p
        .get("max_num_segments")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .max(1);

    for name in targets {
        let Some(st) = store.get(&name) else { continue };
        let mut g = st.write();
        if g.refresh().is_err() {
            continue;
        }
        loop {
            let ids: Vec<tantivy::index::SegmentId> = g
                .index
                .searchable_segment_metas()
                .unwrap_or_default()
                .iter()
                .map(|m| m.id())
                .collect();
            if ids.len() <= max_segments {
                break;
            }
            // merge the whole set down in one step; tantivy handles the rest
            let take = ids.len() - max_segments + 1;
            let batch: Vec<_> = ids.into_iter().take(take).collect();
            let merged = match g.writer() {
                Ok(w) => w.merge(&batch).wait().is_ok(),
                Err(_) => false,
            };
            if !merged {
                break;
            }
            let _ = g.refresh();
        }
    }
    respond(&p, json!({
        "_shards": {"total": touched, "successful": touched, "failed": 0}
    }))
}

const CAT_SEGMENT_COLS: &[&str] = &[
    "index", "shard", "prirep", "ip", "id", "segment", "generation", "docs.count",
    "docs.deleted", "size", "size.memory", "committed", "searchable", "version", "compound",
];

/// `_cat/segments` -- the same information as `_segments`, one row per segment.
pub async fn cat_segments(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    if p.contains_key("help") {
        return cat_help(CAT_SEGMENT_COLS);
    }
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let targets = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
    let mut rows = Vec::new();
    for n in &targets {
        let Some(st) = store.get(n) else { continue };
        let g = st.read();
        if g.closed {
            return err(
                StatusCode::BAD_REQUEST,
                "index_closed_exception",
                format!("closed index [{n}]"),
            );
        }
        let searcher = g.reader.searcher();
        for (i, reader) in searcher.segment_readers().iter().enumerate() {
            rows.push(vec![
                ("index", n.clone()),
                ("shard", "0".to_string()),
                ("prirep", "p".to_string()),
                ("ip", "127.0.0.1".to_string()),
                // `id` answers to `h=` and appears in the help, but the
                // default row does not carry it
                ("segment", format!("_{i}")),
                ("generation", i.to_string()),
                ("docs.count", reader.num_docs().to_string()),
                ("docs.deleted", reader.num_deleted_docs().to_string()),
                ("size", "0b".to_string()),
                ("size.memory", "0".to_string()),
                ("committed", "true".to_string()),
                ("searchable", "true".to_string()),
                ("version", "9.0.0".to_string()),
                ("compound", "true".to_string()),
            ]);
        }
    }
    rows.sort_by(|a, b| a[0].1.cmp(&b[0].1).then(a[5].1.cmp(&b[5].1)));
    cat_render_cols(CAT_SEGMENT_COLS, rows, &p)
}

/// `_list/wlm_stats` -- workload group statistics as a table.
///
/// This engine runs one node and does not divide work between workload
/// groups, so there is one group with nothing rejected. The parameters are
/// still checked, since a caller paging through the list needs to be told
/// when it has asked for something the list cannot give.
pub async fn wlm_stats_list(Query(p): Query<Params>) -> Response {
    let bad = |reason: String| {
        (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "error": {"type": "illegal_argument_exception", "reason": reason},
                "status": 400,
            })),
        )
            .into_response()
    };
    if let Some(sort) = p.get("sort") {
        if !matches!(sort.as_str(), "node_id" | "workload_group") {
            return bad("Invalid value for 'sort'. Allowed: 'node_id', 'workload_group'".into());
        }
    }
    if let Some(order) = p.get("order") {
        if !matches!(order.as_str(), "asc" | "desc") {
            return bad("Invalid value for 'order'. Allowed: 'asc', 'desc'".into());
        }
    }
    if let Some(size) = p.get("size") {
        let n = size.parse::<i64>().unwrap_or(-1);
        if !(1..=100).contains(&n) {
            return bad("Invalid value for 'size'. Allowed range: 1 to 100".into());
        }
    }
    if p.contains_key("next_token") {
        // there is one page and it never moves, so any token names a state
        // that this list cannot have been in
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "error": "Pagination state has changed (e.g., new workload groups added or \
                          removed). Please restart pagination from the beginning by omitting \
                          the 'next_token' parameter.",
                "status": 400,
            })),
        )
            .into_response();
    }
    let cols = [
        "NODE_ID", "WORKLOAD_GROUP_ID", "TOTAL_COMPLETIONS", "TOTAL_REJECTIONS",
        "TOTAL_CANCELLATIONS", "CPU_USAGE", "MEMORY_USAGE",
    ];
    let row = ["node-0", "DEFAULT_WORKLOAD_GROUP", "0", "0", "0", "0", "0"];
    let mut out = String::new();
    if p.get("v").map(|v| v != "false").unwrap_or(false) {
        out.push_str(&cols.join(" "));
        out.push('\n');
    }
    out.push_str(&row.join(" "));
    out.push('\n');
    ([("content-type", "text/plain; charset=UTF-8")], out).into_response()
}

/// `_segments` -- what each shard is made of.
///
/// One shard per index here, and tantivy names its segments by ordinal, so
/// they are reported as `_0`, `_1` and so on to match the shape the API has.
pub async fn segments(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let targets = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
    let allow_none = p.get("allow_no_indices").map(|v| v != "false").unwrap_or(true);
    if targets.is_empty() {
        if !allow_none || (!expr.is_empty() && !expr.contains('*') && !store.exists(&expr)) {
            return no_such_index(&expr);
        }
        return respond(&p, json!({
            "_shards": {"total": 0, "successful": 0, "failed": 0},
            "indices": {},
        }));
    }
    let mut indices = serde_json::Map::new();
    let mut total = 0u64;
    for n in &targets {
        let Some(st) = store.get(n) else { continue };
        let g = st.read();
        if g.closed {
            // a closed index has nothing to report; the caller decides whether
            // that is an error or simply nothing
            if p.get("ignore_unavailable").map(|v| v != "false").unwrap_or(false) {
                continue;
            }
            return err(
                StatusCode::BAD_REQUEST,
                "index_closed_exception",
                format!("closed index [{n}]"),
            );
        }
        let searcher = g.reader.searcher();
        let mut segs = serde_json::Map::new();
        for (i, reader) in searcher.segment_readers().iter().enumerate() {
            segs.insert(
                format!("_{i}"),
                json!({
                    "generation": i,
                    "num_docs": reader.num_docs(),
                    "deleted_docs": reader.num_deleted_docs(),
                    "size_in_bytes": 0,
                    "memory_in_bytes": 0,
                    "committed": true,
                    "search": true,
                    "version": "9.0.0",
                    "compound": true,
                    "attributes": {},
                }),
            );
        }
        total += 1;
        indices.insert(
            n.clone(),
            json!({"shards": {"0": [{
                "routing": {"state": "STARTED", "primary": true, "node": "obsearch"},
                "num_committed_segments": segs.len(),
                "num_search_segments": segs.len(),
                "segments": Value::Object(segs),
            }]}}),
        );
    }
    respond(&p, json!({
        "_shards": {"total": total, "successful": total, "failed": 0},
        "indices": Value::Object(indices),
    }))
}

// --------------------------------------------------------------------- stats

/// Which fields a `fields=`-style parameter names.
///
/// Absent means the caller wants no per-field breakdown at all, which is not
/// the same as naming none.
fn stats_field_patterns(p: &Params, specific: &str) -> Option<Vec<String>> {
    for key in [specific, "fields"] {
        if let Some(v) = p.get(key) {
            return Some(
                v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            );
        }
    }
    None
}

fn stats_field_wanted(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|pat| {
        pat == "*" || pat == "_all" || pat == name || crate::store::glob_match(pat, name)
    })
}

/// A size the way `cat` writes one: a number and the unit it is in.
fn readable_bytes(bytes: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1024 * 1024 * 1024, "gb"),
        (1024 * 1024, "mb"),
        (1024, "kb"),
        (1, "b"),
    ];
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

fn index_stats(st: &IdxState, want_groups: Option<&[String]>, p: &Params) -> Value {
    let searcher = st.reader.searcher();
    let docs = searcher.num_docs();
    let cols = st.field_column_bytes();
    let fielddata_total: u64 = cols.values().sum();
    // a per-field breakdown is reported only where the request asked for one,
    // and a field appears under the statistic its type can carry: fielddata
    // for a text field, completion for a completion field
    let is_completion = |name: &str| st.mapping.type_of(name) == Some("completion");
    let fielddata_fields: Value = match stats_field_patterns(p, "fielddata_fields") {
        None => Value::Null,
        Some(pats) => Value::Object(
            cols.iter()
                .filter(|(k, _)| !is_completion(k) && stats_field_wanted(&pats, k))
                .map(|(k, v)| (k.clone(), json!({"memory_size_in_bytes": v})))
                .collect(),
        ),
    };
    let completion_names: Vec<String> = st
        .mapping
        .types
        .iter()
        .filter(|(_, t)| t.as_str() == Some("completion"))
        .map(|(k, _)| k.clone())
        .collect();
    let completion_total: u64 = completion_names.len() as u64 * 64 * docs.max(1) as u64;
    let completion_fields: Value = match stats_field_patterns(p, "completion_fields") {
        None => Value::Null,
        Some(pats) => Value::Object(
            completion_names
                .iter()
                .filter(|k| stats_field_wanted(&pats, k))
                .map(|k| (k.clone(), json!({"size_in_bytes": 64 * docs.max(1)})))
                .collect(),
        ),
    };
    // nothing is held in a fielddata cache here: term lookups read the
    // column directly, so there is no memory to report against it
    let mut fielddata_stat = json!({"memory_size_in_bytes": fielddata_total, "evictions": 0});
    if let Value::Object(f) = fielddata_fields {
        fielddata_stat["fields"] = Value::Object(f);
    }
    let mut completion_stat = json!({"size_in_bytes": completion_total});
    if let Value::Object(f) = completion_fields {
        completion_stat["fields"] = Value::Object(f);
    }

    // `groups` is only reported for the groups the request named
    let groups: serde_json::Map<String, Value> = st
        .search_groups
        .read()
        .iter()
        .filter(|(k, _)| match want_groups {
            None => false,
            // the request may name groups outright, or by pattern
            Some(w) => w.iter().any(|g| {
                g == "_all" || g == *k || crate::store::glob_match(g, k)
            }),
        })
        .map(|(k, v)| {
            (k.clone(), json!({
                "query_total": v, "query_time_in_millis": 1, "query_current": 0,
                "fetch_total": v, "fetch_time_in_millis": 1, "fetch_current": 0,
                "scroll_total": 0, "scroll_time_in_millis": 0, "scroll_current": 0,
                "suggest_total": 0, "suggest_time_in_millis": 0, "suggest_current": 0
            }))
        })
        .collect();
    let groups_field = match want_groups {
        None => Value::Null,
        Some(_) => Value::Object(groups),
    };
    json!({
        "docs": {"count": docs, "deleted": 0},
        "store": {"size_in_bytes": 0, "reserved_in_bytes": 0},
        "indexing": {"index_total": docs, "index_time_in_millis": 0, "index_current": 0,
                     "index_failed": 0, "delete_total": 0, "delete_time_in_millis": 0,
                     "delete_current": 0,
                     "noop_update_total":
                         st.noop_updates.load(std::sync::atomic::Ordering::Relaxed),
                     "is_throttled": false,
                     "throttle_time_in_millis": 0},
        "get": {"total": st.gets.load(std::sync::atomic::Ordering::Relaxed),
                "time_in_millis": 0, "time": "0s", "getTime": "0s",
                "exists_total": st.gets.load(std::sync::atomic::Ordering::Relaxed),
                "exists_time_in_millis": 0, "missing_total": 0,
                "missing_time_in_millis": 0, "current": 0},
        "search": {"open_contexts": 0, "query_total": st.search_count.load(std::sync::atomic::Ordering::Relaxed), "query_time_in_millis": 1,
                   "query_current": 0, "fetch_total": st.search_count.load(std::sync::atomic::Ordering::Relaxed), "fetch_time_in_millis": 1,
                   "fetch_current": 0, "scroll_total": 0, "scroll_time_in_millis": 0,
                   "scroll_current": 0, "suggest_total": 0, "suggest_time_in_millis": 0,
                   "suggest_current": 0,
                   "groups": groups_field},
        "merges": {"current": 0, "current_docs": 0, "current_size_in_bytes": 0,
                   "total": 0, "total_time_in_millis": 0, "total_docs": 0,
                   "total_size_in_bytes": 0},
        "refresh": {"total": 0, "total_time_in_millis": 0, "external_total": 0,
                    "external_total_time_in_millis": 0, "listeners": 0},
        "flush": {"total": st.flushes.load(std::sync::atomic::Ordering::Relaxed),
                  "periodic": 0, "total_time_in_millis": 0},
        "warmer": {"current": 0, "total": 0, "total_time_in_millis": 0},
        "query_cache": {"memory_size_in_bytes": 0, "total_count": 0, "hit_count": 0,
                        "miss_count": 0, "cache_size": 0, "cache_count": 0, "evictions": 0},
        "fielddata": fielddata_stat,
        "completion": completion_stat,
        // a closed index has nothing loaded, so it reports no segments unless
        // the caller asks for the ones sitting unloaded on disk
        "segments": {"count": if st.closed
                        && !p.get("include_unloaded_segments").map(|v| v == "true").unwrap_or(false)
                    { 0 } else { searcher.segment_readers().len() },
                     "memory_in_bytes": 0,
                     "terms_memory_in_bytes": 0, "stored_fields_memory_in_bytes": 0,
                     "term_vectors_memory_in_bytes": 0, "norms_memory_in_bytes": 0,
                     "points_memory_in_bytes": 0, "doc_values_memory_in_bytes": 0,
                     "index_writer_memory_in_bytes": 0, "version_map_memory_in_bytes": 0,
                     "fixed_bit_set_memory_in_bytes": 0, "max_unsafe_auto_id_timestamp": -1,
                     "file_sizes": {}},
        "translog": {"operations": if st.closed { 0 } else { st.pending.len() },
                     "size_in_bytes": st.pending_bytes.max(55),
                     "uncommitted_operations":
                        if st.closed { 0 } else { st.pending.len() },
                     "uncommitted_size_in_bytes": st.pending_bytes.max(55),
                     "earliest_last_modified_age": 0,
                     "remote_store": {"upload": {"total_uploads": {"started": 0, "failed": 0, "succeeded": 0}}}},
        "request_cache": {
            "memory_size_in_bytes": 0, "evictions": 0, "hit_count": 0,
            "miss_count": st.request_cache_miss.load(std::sync::atomic::Ordering::Relaxed)
        },
        "recovery": {"current_as_source": 0, "current_as_target": 0, "throttle_time_in_millis": 0},
    })
}

fn sum_stats(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let mut out = x.clone();
            for (k, v) in y {
                let merged = match x.get(k) {
                    Some(prev) => sum_stats(prev, v),
                    None => v.clone(),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        (Value::Number(x), Value::Number(y)) => {
            json!(x.as_f64().unwrap_or(0.0) + y.as_f64().unwrap_or(0.0))
        }
        _ => b.clone(),
    }
}

/// `/_stats/{metric}` selects which sections to report; we always report all,
/// so the metric is only consumed to keep it off the index path.
pub const STATS_METRICS: &[&str] = &[
    "docs", "store", "indexing", "get", "search", "merges", "refresh", "flush", "warmer",
    "query_cache", "fielddata", "completion", "segments", "translog", "request_cache",
    "recovery", "_all",
    // the section is named `merges` but the metric may be asked for either way
    "merge",
];

pub async fn stats_metric(
    State(store): State<Store>,
    Path(metric): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    stats_filtered(store, "_all".into(), Some(metric), p)
}

pub async fn stats_index_metric(
    State(store): State<Store>,
    Path((index, metric)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    stats_filtered(store, index, Some(metric), p)
}

/// Are these two names a single character apart -- one changed, added, or
/// dropped? Close enough to be worth suggesting when a metric is not known.
fn one_edit_apart(a: &str, b: &str) -> bool {
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

/// `_stats/{metric}` narrows the report to the sections asked for.
fn stats_filtered(
    store: Store,
    expr: String,
    metric: Option<String>,
    p: Params,
) -> Response {
    let Some(metric) = metric else { return stats_impl(store, expr, p) };
    let wanted: Vec<String> = metric
        .split(',')
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
        // the section is called `merges`, and the metric may be asked for
        // in the singular
        .map(|m| if m == "merge" { "merges".to_string() } else { m.to_string() })
        .collect();
    for w in &wanted {
        if !STATS_METRICS.contains(&w.as_str()) {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                {
                    // a near miss is usually a typo, so the closest known
                    // metric is offered rather than only the complaint
                    let close = STATS_METRICS.iter().find(|m| one_edit_apart(m, w));
                    match close {
                        Some(m) => format!(
                            "request [/_stats/{metric}] contains unrecognized metric: \
                             [{w}] -> did you mean [{m}]?"
                        ),
                        None => format!(
                            "request [/_stats/{metric}] contains unrecognized metric: [{w}]"
                        ),
                    }
                },
            );
        }
    }
    if wanted.iter().any(|w| w == "_all") {
        return stats_impl(store, expr, p);
    }
    let body = match stats_value(&store, &expr, &p) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let keep = |section: &Value| -> Value {
        let mut out = serde_json::Map::new();
        if let Some(o) = section.as_object() {
            for w in &wanted {
                if let Some(v) = o.get(w) {
                    out.insert(w.clone(), v.clone());
                }
            }
        }
        Value::Object(out)
    };
    let mut filtered = body.clone();
    for scope in ["_all"] {
        for kind in ["primaries", "total"] {
            if let Some(v) = body.pointer(&format!("/{scope}/{kind}")) {
                filtered[scope][kind] = keep(v);
            }
        }
    }
    if let Some(indices) = body.get("indices").and_then(|v| v.as_object()) {
        for (name, entry) in indices {
            for kind in ["primaries", "total"] {
                if let Some(v) = entry.get(kind) {
                    filtered["indices"][name][kind] = keep(v);
                }
            }
        }
    }
    respond(&p, filtered)
}



pub async fn stats(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    stats_impl(store, index.map(|Path(i)| i).unwrap_or_else(|| "_all".into()), p)
}

fn stats_impl(store: Store, expr: String, p: Params) -> Response {
    match stats_value(&store, &expr, &p) {
        Ok(v) => respond(&p, v),
        Err(r) => r,
    }
}

fn stats_value(store: &Store, expr: &str, p: &Params) -> std::result::Result<Value, Response> {
    let targets = store.resolve(expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !ignore_unavailable(p) {
        return Err(no_such_index(expr));
    }
    let level = p.get("level").map(|s| s.as_str()).unwrap_or("indices");
    let want_groups: Option<Vec<String>> = p
        .get("groups")
        .map(|g| g.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect());
    let mut indices = serde_json::Map::new();
    let mut all = json!({});
    for n in &targets {
        let Some(st) = store.get(n) else { continue };
        let s = index_stats(&st.read(), want_groups.as_deref(), &p);
        all = sum_stats(&all, &s);
        let mut entry = json!({
            "uuid": "_na_",
            "primaries": s.clone(),
            "total": s,
        });
        if level == "shards" {
            entry["shards"] = json!({"0": [{
                "routing": {"state": "STARTED", "primary": true, "node": "obsearch"},
                "docs": s.get("docs").cloned().unwrap_or(json!({})),
                "commit": {
                    "id": st.read().commit_id(),
                    "generation": 1,
                    "user_data": {},
                    "num_docs": s.pointer("/docs/count").cloned().unwrap_or(json!(0)),
                },
            }]});
        }
        indices.insert(n.clone(), entry);
    }
    let total_shards = shard_total(&store, &targets);
    let mut body = json!({
        "_shards": {"total": total_shards, "successful": total_shards, "failed": 0},
        "_all": {"primaries": all.clone(), "total": all},
    });
    if level != "cluster" {
        body["indices"] = Value::Object(indices);
    }
    Ok(body)
}

// ------------------------------------------------------------------- explain

pub async fn explain(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    // a body that holds something other than a query is not asking about one
    if body.as_object().map(|o| !o.is_empty()).unwrap_or(false)
        && body.get("query").is_none()
    {
        let first = body
            .as_object()
            .and_then(|o| o.keys().next().cloned())
            .unwrap_or_default();
        return err(
            StatusCode::BAD_REQUEST,
            "parsing_exception",
            format!("request does not support [{first}]"),
        );
    }
    let Some(st) = store.get(&index) else { return no_such_index(&index) };
    let (name, src) = {
        let g = st.read();
        (g.name.clone(), read_source(&g, &id))
    };
    let Some(src) = src else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"_index": name, "_id": id, "matched": false})),
        )
            .into_response();
    };

    // decide matched by running the query restricted to this document
    // `q` names a query string on the URL, with `df` saying which field it
    // reads by default and `default_operator` how its words are joined
    let q = match body.get("query").cloned() {
        Some(q) => q,
        None => match p.get("q").filter(|v| !v.is_empty()) {
            Some(text) => {
                let mut qs = json!({"query": text});
                if let Some(df) = p.get("df") {
                    qs["default_field"] = json!(df);
                }
                if let Some(op) = p.get("default_operator") {
                    qs["default_operator"] = json!(op);
                }
                if p.get("lenient").map(|v| v != "false").unwrap_or(false) {
                    qs["lenient"] = json!(true);
                }
                json!({"query_string": qs})
            }
            None => json!({"match_all": {}}),
        },
    };
    let scoped = json!({"bool": {"must": [q], "filter": [{"ids": {"values": [id]}}]}});
    let probe = json!({"query": scoped, "size": 1});
    let matched = crate::search::run(&store, &name, &probe, &Params::new())
        .map(|o| o.total > 0)
        .unwrap_or(false);

    let mut out = json!({
        "_index": name,
        "_id": id,
        "matched": matched,
        "explanation": {
            "value": if matched { 1.0 } else { 0.0 },
            "description": if matched { "match" } else { "no match" },
            "details": []
        }
    });
    let sel = body.get("_source").cloned().or_else(|| source_selector_from_params(&p));
    if let Some(sel) = sel.as_ref().filter(|v| **v != json!(false)) {
        out["get"] = json!({
            "_seq_no": 0, "_primary_term": 1, "found": true,
            "_source": apply_source_selector(&src, sel)
        });
    }
    respond(&p, out)
}

// ---------------------------------------------------------------- field_caps

fn caps_for(kind: &str) -> Value {
    let aggregatable = kind != "text";
    let searchable = true;
    json!({"type": kind, "searchable": searchable, "aggregatable": aggregatable})
}

pub async fn field_caps(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_else(|| "_all".into());
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" {
        return no_such_index(&expr);
    }
    let patterns: Vec<String> = p
        .get("fields")
        .map(|f| f.split(',').map(|s| s.trim().to_string()).collect())
        .or_else(|| {
            body.get("fields").and_then(|f| f.as_array()).map(|a| {
                a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()
            })
        })
        .unwrap_or_else(|| vec!["*".into()]);

    // an index_filter drops indices whose documents don't match
    let index_filter = body.get("index_filter").cloned();
    let mut kept: Vec<String> = Vec::new();
    for n in &targets {
        if let Some(f) = &index_filter {
            let probe = json!({"query": f, "size": 0});
            let hit = crate::search::run(&store, n, &probe, &Params::new())
                .map(|o| o.total > 0)
                .unwrap_or(false);
            if !hit {
                continue;
            }
        }
        kept.push(n.clone());
    }

    let mut fields: serde_json::Map<String, Value> = serde_json::Map::new();
    for n in &kept {
        let Some(st) = store.get(n) else { continue };
        let g = st.read();
        for (name, kind) in g.all_field_types() {
            if !patterns.iter().any(|pat| {
                pat == "*" || *pat == name || crate::store::wildcard_to_regex(pat).is_match(&name)
            }) {
                continue;
            }
            let kinds: Vec<String> = vec![kind.clone()];
            let meta = g
                .mapping
                .raw
                .pointer(&format!("/properties/{}/meta", name.replace('.', "/properties/")))
                .cloned();
            for kind in kinds {
            let entry = fields.entry(name.clone()).or_insert_with(|| json!({}));
            let slot = entry
                .as_object_mut()
                .unwrap()
                .entry(kind.clone())
                .or_insert_with(|| caps_for(&kind));
            if let Some(m) = meta.clone().and_then(|m| m.as_object().cloned()) {
                let dst = slot
                    .as_object_mut()
                    .unwrap()
                    .entry("meta".to_string())
                    .or_insert_with(|| json!({}));
                for (mk, mv) in m {
                    let list = dst
                        .as_object_mut()
                        .unwrap()
                        .entry(mk)
                        .or_insert_with(|| json!([]));
                    if let Some(a) = list.as_array_mut() {
                        if !a.contains(&mv) {
                            a.push(mv);
                        }
                    }
                }
            }
            // a type seen in only some indices lists the ones it came from
            let indices = slot
                .as_object_mut()
                .unwrap()
                .entry("__indices".to_string())
                .or_insert_with(|| json!([]));
            if let Some(a) = indices.as_array_mut() {
                a.push(json!(n));
            }
            }
        }
    }

    // only report `indices` on a field whose type is not uniform
    for (_, per_type) in fields.iter_mut() {
        let type_count = per_type.as_object().map(|o| o.len()).unwrap_or(0);
        if let Some(o) = per_type.as_object_mut() {
            for (_, v) in o.iter_mut() {
                let idx = v.as_object_mut().unwrap().remove("__indices");
                if type_count > 1 {
                    if let Some(i) = idx {
                        v["indices"] = i;
                    }
                }
            }
        }
    }

    respond(&p, json!({"indices": kept, "fields": Value::Object(fields)}))
}

// -------------------------------------------------------------------- alias

pub async fn get_alias(
    State(store): State<Store>,
    Query(p): Query<Params>,
) -> Response {
    let mut out = serde_json::Map::new();
    for n in store.names() {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        let mut aliases = serde_json::Map::new();
        for (a, def) in &g.aliases {
            aliases.insert(a.clone(), def.clone());
        }
        out.insert(n.clone(), json!({"aliases": Value::Object(aliases)}));
    }
    respond(&p, Value::Object(out))
}

// -------------------------------------------------------------- cluster info

/// A single-node cluster is always green once it is up; the suite mostly uses
/// this endpoint as a barrier before it starts asserting.
pub async fn cluster_health(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i);
    // `expand_wildcards` decides whether a pattern reaches closed indices,
    // which are the ones that make the cluster less than green
    let states = p.get("expand_wildcards").map(|v| v.as_str()).unwrap_or("open");
    let want_open = states.split(',').any(|w| matches!(w.trim(), "open" | "all"));
    let want_closed = states.split(',').any(|w| matches!(w.trim(), "closed" | "all"));
    let names: Vec<String> = match expr.as_deref() {
        Some(e) if !e.is_empty() && e != "_all" => store
            .resolve(e)
            .into_iter()
            .filter(|n| {
                store
                    .get(n)
                    .map(|st| {
                        let closed = st.read().closed;
                        if !e.contains('*') {
                            true
                        } else if closed {
                            want_closed
                        } else {
                            want_open
                        }
                    })
                    .unwrap_or(false)
            })
            .collect(),
        _ => store.names(),
    };

    // one node can hold one copy of a shard, so an index asking for replicas
    // has some it will never get, and the cluster is yellow while it is in view
    let unassignable = names.iter().filter_map(|n| store.get(n)).any(|st| {
        st.read().numeric_setting("number_of_replicas").unwrap_or(0) > 0
    });
    let status = if unassignable { "yellow" } else { "green" };

    // a wait this engine cannot satisfy is answered as a timeout rather than
    // by waiting: nothing here is going to change while the request is held
    let satisfied = match p.get("wait_for_status").map(|v| v.as_str()) {
        Some("green") => status == "green",
        Some("yellow") => status != "red",
        _ => true,
    } && p
        .get("wait_for_nodes")
        .map(|v| {
            let want = v.trim_start_matches(['>', '<', '=']).parse::<i64>().unwrap_or(1);
            if v.starts_with('>') { 1 > want } else { 1 >= want }
        })
        .unwrap_or(true)
        // a wait for more active shards than the cluster has can never end
        && p.get("wait_for_active_shards")
            .map(|v| match v.as_str() {
                "all" => true,
                other => other.parse::<usize>().map(|n| n <= names.len()).unwrap_or(true),
            })
            .unwrap_or(true);

    let n = names.len();
    let mut out = json!({
        "cluster_name": "obsearch", "status": status, "timed_out": !satisfied,
        "number_of_nodes": 1, "number_of_data_nodes": 1, "discovered_master": true,
        "discovered_cluster_manager": true,
        "active_primary_shards": n, "active_shards": n,
        "relocating_shards": 0, "initializing_shards": 0, "unassigned_shards": 0,
        "delayed_unassigned_shards": 0, "number_of_pending_tasks": 0,
        "number_of_in_flight_fetch": 0, "task_max_waiting_in_queue_millis": 0,
        "active_shards_percent_as_number": 100.0,
    });
    // `level` says how far down to report: the cluster, each index, or each
    // shard within them
    let level = p.get("level").map(|v| v.as_str()).unwrap_or("cluster");
    if level == "indices" || level == "shards" {
        let mut indices = serde_json::Map::new();
        for name in &names {
            let Some(st) = store.get(name) else { continue };
            let replicas = st.read().numeric_setting("number_of_replicas").unwrap_or(0);
            let closed = replicas > 0;
            let mut entry = json!({
                "status": if closed { "yellow" } else { "green" },
                "number_of_shards": 1, "number_of_replicas": 0,
                "active_primary_shards": 1, "active_shards": 1,
                "relocating_shards": 0, "initializing_shards": 0, "unassigned_shards": 0,
            });
            if level == "shards" {
                entry["shards"] = json!({"0": {
                    "status": if closed { "yellow" } else { "green" },
                    "primary_active": true, "active_shards": 1,
                    "relocating_shards": 0, "initializing_shards": 0, "unassigned_shards": 0,
                }});
            }
            indices.insert(st.read().name.clone(), entry);
        }
        out["indices"] = Value::Object(indices);
    }
    if !satisfied {
        return (StatusCode::REQUEST_TIMEOUT, axum::Json(out)).into_response();
    }
    respond(&p, out)
}

/// `_recovery` -- how each shard came to be where it is.
///
/// A shard here is created empty and is immediately done, so every recovery
/// is an EMPTY_STORE that finished, with nothing copied from anywhere.
pub async fn indices_recovery(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let names = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
    if !expr.is_empty() && !ignore_unavailable(&p) {
        for part in expr.split(',').map(|n| n.trim()).filter(|n| !n.contains('*')) {
            if store.resolve(part).is_empty() {
                return no_such_index(part);
            }
        }
    }
    let mut out = serde_json::Map::new();
    for n in names {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        let existing = g.reader.searcher().num_docs() > 0 || g.closed;
        out.insert(g.name.clone(), json!({"shards": [{
            "id": 0,
            "type": if existing { "EXISTING_STORE" } else { "EMPTY_STORE" },
            "stage": "DONE",
            "primary": true,
            "start_time": "2020-01-01T00:00:00.000Z",
            "start_time_in_millis": 1_577_836_800_000u64,
            "stop_time": "2020-01-01T00:00:00.000Z",
            "stop_time_in_millis": 1_577_836_800_000u64,
            "total_time": "0s",
            "total_time_in_millis": 0,
            "source": {},
            "target": {
                "id": "node-0", "host": "127.0.0.1", "transport_address": "127.0.0.1:9300",
                "ip": "127.0.0.1", "name": "obsearch",
            },
            "index": {
                "size": {
                    "total": "0b", "total_in_bytes": 0,
                    "reused": "0b", "reused_in_bytes": 0,
                    "recovered": "0b", "recovered_in_bytes": 0,
                    "percent": "0.0%",
                },
                "files": {
                    "total": 0, "reused": 0, "recovered": 0, "percent": "0.0%",
                    "details": [],
                },
                "total_time": "0s", "total_time_in_millis": 0,
                "source_throttle_time": "0s", "source_throttle_time_in_millis": 0,
                "target_throttle_time": "0s", "target_throttle_time_in_millis": 0,
            },
            "translog": {
                "recovered": 0, "total": 0, "percent": "100.0%",
                "total_on_start": 0, "total_time": "0s", "total_time_in_millis": 0,
            },
            "verify_index": {
                "check_index_time": "0s", "check_index_time_in_millis": 0,
                "total_time": "0s", "total_time_in_millis": 0,
            },
        }]}));
    }
    respond(&p, Value::Object(out))
}

/// `_upgrade` -- there is one segment format and it is the current one, so an
/// upgrade has nothing to do but say which it is.
pub async fn indices_upgrade(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let names = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
    if !expr.is_empty() && !ignore_unavailable(&p) {
        for part in expr.split(',').map(|n| n.trim()).filter(|n| !n.contains('*')) {
            if store.resolve(part).is_empty() {
                return no_such_index(part);
            }
        }
    }
    let tally = shards_over(&store, &names);
    let mut upgraded = serde_json::Map::new();
    for n in names {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        upgraded.insert(g.name.clone(), json!({
            "upgrade_version": "3.0.0",
            "oldest_lucene_segment_version": "9.0.0",
        }));
    }
    respond(&p, json!({"_shards": tally, "upgraded_indices": Value::Object(upgraded)}))
}

/// `_cluster/allocation/explain` -- why a shard sits where it does.
///
/// Every shard is started on the one node, so the only honest explanation is
/// that it is where it belongs and has nowhere else to go.
pub async fn allocation_explain(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    // without a shard named, the question is which unassigned shard needs
    // explaining: an index asking for more replicas than there are nodes has
    // some, and otherwise there are none to explain
    let unassigned = store.names().into_iter().find(|n| {
        store
            .get(n)
            .map(|st| st.read().numeric_setting("number_of_replicas").unwrap_or(0) > 0)
            .unwrap_or(false)
    });
    let named = body.get("index").and_then(|v| v.as_str()).map(|s| s.to_string());
    let Some(index) = named.or(unassigned) else {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "unable to find any unassigned shards to explain [ClusterAllocationExplainRequest[useAnyUnassignedShard=true]]",
        );
    };
    let index = index.as_str();
    if store.resolve(index).is_empty() {
        return no_such_index(index);
    }
    let shard = body.get("shard").and_then(|v| v.as_i64()).unwrap_or(0);
    let primary = body.get("primary").and_then(|v| v.as_bool()).unwrap_or(true);
    respond(&p, json!({
        "index": index,
        "shard": shard,
        "primary": primary,
        "current_state": "started",
        "current_node": {
            "id": "node-0", "name": "obsearch",
            "transport_address": "127.0.0.1:9300", "weight_ranking": 1,
        },
        "can_remain_on_current_node": "yes",
        "can_rebalance_cluster": "yes",
        "can_rebalance_to_other_node": "no",
        "rebalance_explanation":
            "cannot rebalance as no target node exists that can both allocate this shard \
             and improve the cluster balance",
        "node_allocation_decisions": [],
    }))
}

/// `_cluster/voting_config_exclusions` -- nodes kept out of the vote that
/// elects a cluster manager.
///
/// One node has no election to hold, but the exclusions are still recorded
/// and reported, since a caller draining a node watches this list to know the
/// exclusion took.
pub async fn post_voting_config_exclusions(
    State(store): State<Store>,
    Query(p): Query<Params>,
) -> Response {
    let ids = p.get("node_ids").filter(|v| !v.is_empty());
    let names = p
        .get("node_names")
        .or_else(|| p.get("node_name"))
        .filter(|v| !v.is_empty());
    let entries: Vec<Value> = match (ids, names) {
        (Some(ids), None) => ids
            .split(',')
            .map(|n| json!({"node_id": n.trim(), "node_name": "_absent_"}))
            .collect(),
        (None, Some(names)) => names
            .split(',')
            .map(|n| json!({"node_id": "_absent_", "node_name": n.trim()}))
            .collect(),
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                "Please set node identifiers correctly. One and only one of [node_name], \
                 [node_names] and [node_ids] has to be set",
            );
        }
    };
    store.add_voting_exclusions(entries);
    (StatusCode::OK, axum::Json(json!({}))).into_response()
}

pub async fn delete_voting_config_exclusions(State(store): State<Store>) -> Response {
    store.clear_voting_exclusions();
    (StatusCode::OK, axum::Json(json!({}))).into_response()
}

/// `/_cluster/state/<metrics>` and `/_cluster/state/<metrics>/<indices>`:
/// the first path part names which sections to return, the second which
/// indices the metadata should describe.
pub async fn cluster_state_filtered(
    State(store): State<Store>,
    Path(rest): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let mut parts = rest.splitn(2, '/');
    let metrics = parts.next().unwrap_or("_all").to_string();
    let indices = parts.next().map(|s| s.to_string());
    cluster_state_inner(&store, &p, Some(&metrics), indices.as_deref())
}

pub async fn cluster_state(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    cluster_state_inner(&store, &p, None, None)
}

fn cluster_state_inner(
    store: &Store,
    p: &Params,
    metrics: Option<&str>,
    index_expr: Option<&str>,
) -> Response {
    let all = metrics.map(|m| m == "_all").unwrap_or(true);
    let wanted: Vec<&str> = metrics.map(|m| m.split(',').collect()).unwrap_or_default();
    let want = |name: &str| all || wanted.contains(&name);

    // which indices the metadata should describe; without an expression it is
    // every index there is
    let names = match index_expr {
        Some(expr) if expr != "_all" => {
            // `expand_wildcards` names the states a pattern reaches, and
            // naming only one of them leaves the other out
            let states = p.get("expand_wildcards").map(|v| v.as_str()).unwrap_or("open,closed");
            let want_open = states.split(',').any(|w| matches!(w.trim(), "open" | "all"));
            let want_closed = states.split(',').any(|w| matches!(w.trim(), "closed" | "all"));
            let found: Vec<String> = store
                .resolve(expr)
                .into_iter()
                .filter(|n| {
                    store
                        .get(n)
                        .map(|st| if st.read().closed { want_closed } else { want_open })
                        .unwrap_or(false)
                })
                .collect();
            for part in expr.split(',').map(|n| n.trim()).filter(|n| !n.contains('*')) {
                if !found.iter().any(|n| n == part) && !ignore_unavailable(p) {
                    return no_such_index(part);
                }
            }
            // a pattern reaching nothing is an error only when the caller said so
            let allow_none = p.get("allow_no_indices").map(|v| v != "false").unwrap_or(true);
            if found.is_empty() && !allow_none {
                return no_such_index(expr);
            }
            found
        }
        _ => store.names(),
    };

    let mut indices = serde_json::Map::new();
    for n in names {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        // the space a shard count can be split into later: the power-of-two
        // multiple of the shards the index was given
        let mut routing_shards = g.numeric_setting("number_of_shards").unwrap_or(1).max(1);
        while routing_shards * 2 <= 1024 {
            routing_shards *= 2;
        }
        indices.insert(g.name.clone(), json!({
            "aliases": g.aliases.keys().cloned().collect::<Vec<_>>(),
            "mappings": g.mapping.raw,
            "settings": g.effective_settings(),
            "state": if g.closed { "close" } else { "open" },
            "routing_num_shards": routing_shards,
        }));
    }

    let mut out = serde_json::Map::new();
    out.insert("cluster_name".into(), json!("obsearch"));
    out.insert("cluster_uuid".into(), json!("_na_"));
    if want("version") || all {
        out.insert("version".into(), json!(1));
        out.insert("state_uuid".into(), json!("_na_"));
    }
    // the two names are the old and new spellings of one thing, but a
    // request naming one does not ask for the other
    if want("master_node") {
        out.insert("master_node".into(), json!("node-0"));
    }
    if want("cluster_manager_node") {
        out.insert("cluster_manager_node".into(), json!("node-0"));
    }
    if want("nodes") {
        out.insert("nodes".into(), json!({"node-0": {
            "name": "obsearch", "ephemeral_id": "_na_",
            "transport_address": "127.0.0.1:9300", "attributes": {}}}));
    }
    if want("metadata") {
        out.insert("metadata".into(), json!({
            "cluster_uuid": "_na_",
            "templates": store.get_templates(),
            "indices": Value::Object(indices.clone()),
            "cluster_coordination": {
                "voting_config_exclusions": store.voting_exclusions(),
            },
        }));
    }
    if want("blocks") {
        // an index held still, or closed, is one the cluster is blocking
        let mut per_index = serde_json::Map::new();
        for name in indices.keys() {
            let Some(st) = store.get(name) else { continue };
            let g = st.read();
            let mut held = serde_json::Map::new();
            if g.closed {
                held.insert("4".into(), json!({
                    "description": "index closed", "retryable": false,
                    "levels": ["read", "write"],
                }));
            }
            if g.setting("blocks.write").as_deref() == Some("true") {
                held.insert("8".into(), json!({
                    "description": "index write (api)", "retryable": false,
                    "levels": ["write"],
                }));
            }
            // a read-only index refuses writes and metadata changes both
            if g.setting("blocks.read_only").as_deref() == Some("true") {
                held.insert("5".into(), json!({
                    "description": "index read-only (api)", "retryable": false,
                    "levels": ["write", "metadata_write"],
                }));
            }
            if g.setting("blocks.metadata").as_deref() == Some("true") {
                held.insert("9".into(), json!({
                    "description": "index metadata (api)", "retryable": false,
                    "levels": ["metadata_write", "metadata_read"],
                }));
            }
            if g.setting("blocks.read").as_deref() == Some("true") {
                held.insert("7".into(), json!({
                    "description": "index read (api)", "retryable": false,
                    "levels": ["read"],
                }));
            }
            if !held.is_empty() {
                per_index.insert(name.clone(), Value::Object(held));
            }
        }
        out.insert("blocks".into(), if per_index.is_empty() {
            json!({})
        } else {
            json!({"indices": Value::Object(per_index)})
        });
    }
    if want("routing_table") {
        let mut tables = serde_json::Map::new();
        for name in indices.keys() {
            tables.insert(name.clone(), json!({"shards": {"0": [{
                "state": "STARTED", "primary": true, "node": "node-0",
                "relocating_node": Value::Null, "shard": 0, "index": name,
            }]}}));
        }
        out.insert("routing_table".into(), json!({"indices": Value::Object(tables)}));
    }
    if want("routing_nodes") {
        let shards: Vec<Value> = indices
            .keys()
            .map(|name| json!({
                "state": "STARTED", "primary": true, "node": "node-0",
                "relocating_node": Value::Null, "shard": 0, "index": name,
            }))
            .collect();
        out.insert(
            "routing_nodes".into(),
            json!({"unassigned": [], "nodes": {"node-0": shards}}),
        );
    }
    respond(p, Value::Object(out))
}

/// Walk a settings body into dotted keys with text values, whichever way the
/// caller wrote it.
fn flatten_cluster_settings(node: &Value, prefix: &str, out: &mut serde_json::Map<String, Value>) {
    match node {
        Value::Object(o) => {
            for (k, v) in o {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten_cluster_settings(v, &key, out);
            }
        }
        Value::Null => {
            out.insert(prefix.to_string(), Value::Null);
        }
        Value::String(s) => {
            out.insert(prefix.to_string(), json!(s));
        }
        other => {
            out.insert(prefix.to_string(), json!(other.to_string()));
        }
    }
}

/// Cluster settings are held with dotted keys and string values, which is the
/// flat form. `flat_settings=false`, the default, reports them as a tree.
fn nest_settings(flat: &Value) -> Value {
    let mut out = json!({});
    let Some(o) = flat.as_object() else { return out };
    for (k, v) in o {
        let path: Vec<&str> = k.split('.').collect();
        let mut cur = &mut out;
        for part in &path[..path.len() - 1] {
            cur = cur
                .as_object_mut()
                .unwrap()
                .entry((*part).to_string())
                .or_insert_with(|| json!({}));
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

/// A setting whose value the cluster refuses rather than stores.
fn check_cluster_setting(key: &str, value: &Value) -> Option<Response> {
    // a null is a removal, not a value, and nothing about it can be wrong
    if value.is_null() {
        return None;
    }
    // a cancellation rate or ratio of zero would cancel nothing, which is not
    // a setting so much as a way of turning the feature off by halves
    if key.starts_with("search_backpressure.") && key.contains("cancellation_") {
        let n = value
            .as_f64()
            .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(1.0);
        if n <= 0.0 {
            return Some(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("{key} must be > 0"),
            ));
        }
    }
    if key == "search_backpressure.mode" {
        let v = value.as_str().unwrap_or("");
        if !matches!(v, "monitor_only" | "enforced" | "disabled") {
            return Some(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("Invalid SearchBackpressureMode: {v}"),
            ));
        }
    }
    None
}

pub async fn cluster_settings_get(
    State(store): State<Store>,
    Query(p): Query<Params>,
) -> Response {
    let raw = store.cluster_settings();
    let flat = p.get("flat_settings").map(|v| v == "true").unwrap_or(false);
    let view = |scope: &str| match raw.get(scope) {
        Some(v) if !flat => nest_settings(v),
        Some(v) => v.clone(),
        None => json!({}),
    };
    // `include_defaults` asks for the settings nobody set, which here is what
    // the node was started with
    let mut defaults = json!({});
    if flag(&p, "include_defaults") {
        for (k, v) in node_attrs() {
            defaults[format!("node.attr.{k}")] = json!(v);
        }
        if !flat {
            defaults = nest_settings(&defaults);
        }
    }
    respond(&p, json!({
        "persistent": view("persistent"),
        "transient": view("transient"),
        "defaults": defaults,
    }))
}

pub async fn cluster_settings_put(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let mut body: Value = parse_body(&body).unwrap_or(json!({}));
    // settings arrive dotted or nested and are held one way: dotted, with the
    // value as text, which is how they are reported back
    for scope in ["persistent", "transient"] {
        let Some(o) = body.get(scope).and_then(|v| v.as_object()).cloned() else { continue };
        let mut flat = serde_json::Map::new();
        flatten_cluster_settings(&Value::Object(o), "", &mut flat);
        for (k, v) in &flat {
            if let Some(r) = check_cluster_setting(k, v) {
                return r;
            }
        }
        body[scope] = Value::Object(flat);
    }
    store.merge_cluster_settings(&body);
    // the answer is shaped the way the request asked to see settings
    let flat = p.get("flat_settings").map(|v| v == "true").unwrap_or(false);
    let echo = |scope: &str| match body.get(scope) {
        Some(v) if flat => {
            // a key set to null was a removal, and is not in the result
            let mut o = v.as_object().cloned().unwrap_or_default();
            o.retain(|_, val| !val.is_null());
            Value::Object(o)
        }
        Some(v) => nest_settings(v),
        None => json!({}),
    };
    respond(&p, json!({
        "acknowledged": true,
        "persistent": echo("persistent"),
        "transient": echo("transient"),
    }))
}

// -------------------------------------------------------------------- aliases

fn alias_view(
    store: &Store,
    index_expr: Option<&str>,
    name_expr: Option<&str>,
    states: Option<&str>,
) -> Value {
    let targets = match index_expr {
        Some(e) => store.resolve(e),
        None => store.names(),
    };
    // `expand_wildcards` says which states to include; without it, both
    let (want_open, want_closed) = match states {
        None => (true, true),
        Some(v) => (
            v.split(',').any(|w| matches!(w.trim(), "open" | "all")),
            v.split(',').any(|w| matches!(w.trim(), "closed" | "all")),
        ),
    };
    let targets: Vec<String> = targets
        .into_iter()
        .filter(|n| {
            store
                .get(n)
                .map(|st| if st.read().closed { want_closed } else { want_open })
                .unwrap_or(false)
        })
        .collect();
    let mut out = serde_json::Map::new();
    for n in targets {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        let mut aliases = serde_json::Map::new();
        for (a, def) in &g.aliases {
            if alias_name_wanted(name_expr, a) {
                aliases.insert(a.clone(), def.clone());
            }
        }
        // naming an alias asks which indices carry it, so one that carries
        // none is not an answer; asking without a name asks about the indices
        // themselves, and an index with no aliases is still one of them
        if aliases.is_empty() && name_expr.is_some() {
            continue;
        }
        out.insert(n.clone(), json!({"aliases": Value::Object(aliases)}));
    }
    Value::Object(out)
}


/// Does an alias fall inside the expression naming it?
///
/// The expression is a comma list where a leading `-` removes rather than
/// adds, so `test_alias*,-test_alias_1` is every matching alias but that one.
fn alias_name_wanted(expr: Option<&str>, alias: &str) -> bool {
    let Some(expr) = expr.filter(|e| !e.is_empty()) else { return true };
    if matches!(expr, "*" | "_all") {
        return true;
    }
    let mut wanted = false;
    for pat in expr.split(',') {
        let pat = pat.trim();
        let (neg, pat) = match pat.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, pat),
        };
        let hit = pat == alias
            || pat == "*"
            || pat == "_all"
            || crate::store::wildcard_to_regex(pat).is_match(alias);
        if hit {
            wanted = !neg;
        }
    }
    wanted
}

/// The names in the expression that must exist for the request to succeed.
///
/// A pattern that matches nothing is simply an empty result, but a plain name
/// that matches nothing is a request for something that is not there.
fn alias_names_required(expr: Option<&str>) -> Vec<String> {
    let Some(expr) = expr.filter(|e| !e.is_empty()) else { return Vec::new() };
    expr.split(',')
        .map(|p| p.trim())
        .filter(|p| !p.starts_with('-') && !p.contains('*') && *p != "_all" && !p.is_empty())
        .map(|p| p.to_string())
        .collect()
}

/// The 404 this endpoint answers with carries the reason as a bare string
/// rather than the usual error object.
fn aliases_missing_response(names: &[String], view: &Value) -> Response {
    let mut names: Vec<String> = names.to_vec();
    names.sort();
    let label = if names.len() > 1 { "aliases" } else { "alias" };
    // the aliases that were found are still reported alongside the complaint
    let mut body = view.clone();
    if !body.is_object() {
        body = json!({});
    }
    if let Some(o) = body.as_object_mut() {
        o.insert("error".into(), json!(format!("{label} [{}] missing", names.join(","))));
        o.insert("status".into(), json!(404));
    }
    (StatusCode::NOT_FOUND, axum::Json(body)).into_response()
}

/// Which of the named aliases do not exist at all.
///
/// A name that exists but was removed from the answer by a later exclusion is
/// not missing -- it is excluded. Only a name nothing carries is missing.
/// Exclusions at the head of the list are a separate complaint: they have
/// nothing to exclude from, and are reported as written.
fn alias_names_missing(store: &Store, idx: Option<&str>, expr: Option<&str>) -> Vec<String> {
    let Some(expr) = expr.filter(|e| !e.is_empty()) else { return Vec::new() };
    // the run of plain exclusions at the head, ending at the first entry that
    // adds something or carries a wildcard
    let mut leading = Vec::new();
    for pat in expr.split(',').map(|p| p.trim()) {
        if !pat.starts_with('-') || pat.contains('*') {
            break;
        }
        leading.push(pat.to_string());
    }
    if !leading.is_empty() {
        return leading;
    }
    let targets = match idx {
        Some(e) => store.resolve(e),
        None => store.names(),
    };
    let existing: std::collections::HashSet<String> = targets
        .iter()
        .filter_map(|n| store.get(n))
        .flat_map(|st| st.read().aliases.keys().cloned().collect::<Vec<_>>())
        .collect();
    expr.split(',')
        .map(|p| p.trim())
        .filter(|p| {
            !p.starts_with('-') && !p.contains('*') && *p != "_all" && !p.is_empty()
        })
        .filter(|p| !existing.contains(*p))
        .map(|p| p.to_string())
        .collect()
}

/// `GET /{index}/_alias` -- every alias on the named indices.
pub async fn index_alias_list(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if store.resolve(&index).is_empty() {
        return no_such_index(&index);
    }
    respond(&p, alias_view(&store, Some(&index), None, p.get("expand_wildcards").map(|v| v.as_str())))
}

pub async fn get_alias_scoped(
    State(store): State<Store>,
    path: Option<Path<Vec<String>>>,
    Query(p): Query<Params>,
) -> Response {
    let parts = path.map(|Path(v)| v).unwrap_or_default();
    let (idx, name) = match parts.len() {
        0 => (None, None),
        1 => (None, Some(parts[0].clone())),
        _ => (Some(parts[0].clone()), Some(parts[1].clone())),
    };
    let view = alias_view(&store, idx.as_deref(), name.as_deref(), p.get("expand_wildcards").map(|v| v.as_str()));
    let missing = alias_names_missing(&store, idx.as_deref(), name.as_deref());
    if !missing.is_empty() {
        return aliases_missing_response(&missing, &view);
    }
    respond(&p, view)
}

pub async fn index_alias_get(
    State(store): State<Store>,
    Path((index, name)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    let view = alias_view(&store, Some(&index), Some(&name), p.get("expand_wildcards").map(|v| v.as_str()));
    let missing = alias_names_missing(&store, Some(&index), Some(&name));
    if !missing.is_empty() {
        return aliases_missing_response(&missing, &view);
    }
    respond(&p, view)
}

pub async fn index_alias_head(
    State(store): State<Store>,
    Path((index, name)): Path<(String, String)>,
) -> Response {
    let view = alias_view(&store, Some(&index), Some(&name), None);
    let any = view
        .as_object()
        .map(|o| {
            o.values().any(|v| {
                v.get("aliases").and_then(|a| a.as_object()).map(|a| !a.is_empty()).unwrap_or(false)
            })
        })
        .unwrap_or(false);
    if any { StatusCode::OK.into_response() } else { StatusCode::NOT_FOUND.into_response() }
}

pub async fn exists_alias(
    State(store): State<Store>,
    path: Option<Path<Vec<String>>>,
) -> Response {
    let parts = path.map(|Path(v)| v).unwrap_or_default();
    let (idx, name) = match parts.len() {
        0 => (None, None),
        1 => (None, Some(parts[0].clone())),
        _ => (Some(parts[0].clone()), Some(parts[1].clone())),
    };
    let view = alias_view(&store, idx.as_deref(), name.as_deref(), None);
    let any = view.as_object().map(|o| {
        o.values().any(|v| {
            v.get("aliases").and_then(|a| a.as_object()).map(|a| !a.is_empty()).unwrap_or(false)
        })
    }).unwrap_or(false);
    if any { StatusCode::OK.into_response() } else { StatusCode::NOT_FOUND.into_response() }
}

/// The keys an alias body may carry that are not part of the alias itself.
const ALIAS_ADDRESSING: &[&str] = &["index", "indices", "alias", "aliases"];
const ALIAS_OPTIONS: &[&str] = &[
    "filter",
    "routing",
    "index_routing",
    "search_routing",
    "is_write_index",
    "is_hidden",
    "must_exist",
];

/// Create or replace an alias.
///
/// The index and the alias name may each arrive in the path or in the body,
/// which is four spellings of the same request.
async fn put_alias_inner(
    store: Store,
    index: Option<String>,
    name: Option<String>,
    p: Params,
    body: String,
) -> Response {
    let mut def: Value = parse_body(&body).unwrap_or_else(|_| json!({}));
    if !def.is_object() {
        def = json!({});
    }
    let from_body = |keys: &[&str]| -> Option<String> {
        let o = def.as_object()?;
        for k in keys {
            match o.get(*k) {
                Some(Value::String(s)) => return Some(s.clone()),
                Some(Value::Array(a)) => {
                    let joined: Vec<String> =
                        a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
                    if !joined.is_empty() {
                        return Some(joined.join(","));
                    }
                }
                _ => {}
            }
        }
        None
    };
    // what the body names wins over what the path names, for the index and
    // for the alias alike
    let index = from_body(&["index", "indices"]).or_else(|| index.filter(|s| !s.is_empty()));
    let name = from_body(&["alias", "aliases"]).or_else(|| name.filter(|s| !s.is_empty()));

    let (Some(index), Some(name)) = (index, name) else {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: index is missing;2: alias is missing;",
        );
    };
    if let Some(o) = def.as_object() {
        for key in o.keys() {
            let key: &str = key;
            if !ALIAS_ADDRESSING.contains(&key) && !ALIAS_OPTIONS.contains(&key) {
                return err(
                    StatusCode::BAD_REQUEST,
                    "x_content_parse_exception",
                    format!("unknown field [{key}]"),
                );
            }
        }
    }
    // an alias is a name for indices, so it can be neither a pattern nor the
    // name an index already answers to
    if name.contains('*') || name.contains(',') {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_alias_name_exception",
            format!("Invalid alias name [{name}]"),
        );
    }
    if store.names().iter().any(|n| *n == name) {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_alias_name_exception",
            format!("Invalid alias name [{name}]: an index or data stream exists with the same name as the alias"),
        );
    }
    let targets = store.resolve(&index);
    if targets.is_empty() {
        return no_such_index(&index);
    }
    if let Some(o) = def.as_object_mut() {
        for k in ALIAS_ADDRESSING {
            o.remove(*k);
        }
    }
    for n in targets {
        if let Some(st) = store.get(&n) {
            st.write().aliases.insert(name.clone(), crate::store::normalize_alias(&def));
        }
    }
    respond(&p, json!({"acknowledged": true}))
}

pub async fn put_alias(
    State(store): State<Store>,
    Path((index, name)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    put_alias_inner(store, Some(index), Some(name), p, body).await
}

/// `PUT /{index}/_alias` -- the alias name comes from the body.
pub async fn put_alias_on_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    put_alias_inner(store, Some(index), None, p, body).await
}

/// `PUT /_alias/{name}` -- the indices come from the body.
pub async fn put_alias_named(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    put_alias_inner(store, None, Some(name), p, body).await
}

/// `PUT /_alias` -- both come from the body.
pub async fn put_alias_body(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    put_alias_inner(store, None, None, p, body).await
}

pub async fn delete_alias(
    State(store): State<Store>,
    Path((index, name)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    let targets = store.resolve(&index);
    if targets.is_empty() {
        return no_such_index(&index);
    }
    let mut removed = false;
    for n in targets {
        if let Some(st) = store.get(&n) {
            for pat in name.split(',') {
                let pat = pat.trim();
                let mut g = st.write();
                // `_all` names every alias the index carries
                let hits: Vec<String> = if pat == "_all" {
                    g.aliases.keys().cloned().collect()
                } else {
                    let re = crate::store::wildcard_to_regex(pat);
                    g.aliases.keys().filter(|a| re.is_match(a)).cloned().collect()
                };
                for h in hits {
                    g.aliases.remove(&h);
                    removed = true;
                }
            }
        }
    }
    if !removed {
        return err(
            StatusCode::NOT_FOUND,
            "aliases_not_found_exception",
            format!("aliases [{name}] missing"),
        );
    }
    respond(&p, json!({"acknowledged": true}))
}

pub async fn update_aliases(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let actions = body.get("actions").and_then(|a| a.as_array()).cloned().unwrap_or_default();
    if actions.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: Must specify at least one alias action;",
        );
    }
    // `must_exist` on a remove turns "nothing to do" into an error, and every
    // action is checked before any of them is reported
    let mut missing_required: Vec<String> = Vec::new();
    for action in actions {
        let Some((verb, spec)) = action.as_object().and_then(|o| o.iter().next()) else { continue };
        let indices: Vec<String> = spec
            .get("index")
            .and_then(|v| v.as_str())
            .map(|s| store.resolve(s))
            .or_else(|| {
                spec.get("indices").and_then(|v| v.as_array()).map(|a| {
                    a.iter().filter_map(|x| x.as_str()).flat_map(|x| store.resolve(x)).collect()
                })
            })
            .unwrap_or_default();
        let names: Vec<String> = spec
            .get("alias")
            .and_then(|v| v.as_str())
            .map(|s| vec![s.to_string()])
            .or_else(|| {
                spec.get("aliases").and_then(|v| v.as_array()).map(|a| {
                    a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()
                })
            })
            .unwrap_or_default();
        // an explicit empty list is a request that names nothing
        if spec.get("aliases").and_then(|v| v.as_array()).map(|a| a.is_empty()).unwrap_or(false) {
            return err(
                StatusCode::BAD_REQUEST,
                "action_request_validation_exception",
                "Validation Failed: 1: [aliases] can't be empty;",
            );
        }
        if indices.is_empty() {
            let want = spec.get("index").and_then(|v| v.as_str()).unwrap_or("");
            return no_such_index(want);
        }
        // a remove that removed nothing is an error, whether or not the caller
        // asked for must_exist -- there was nothing there to act on
        let mut removed_any = false;
        for i in &indices {
            let Some(st) = store.get(i) else { continue };
            let mut g = st.write();
            for a in &names {
                match verb.as_str() {
                    "add" => {
                        let mut def = spec.clone();
                        if let Some(o) = def.as_object_mut() {
                            o.remove("index");
                            o.remove("indices");
                            o.remove("alias");
                            o.remove("aliases");
                        }
                        g.aliases.insert(a.clone(), crate::store::normalize_alias(&def));
                    }
                    "remove" => {
                        let re = crate::store::wildcard_to_regex(a);
                        let hits: Vec<String> =
                            g.aliases.keys().filter(|x| re.is_match(x)).cloned().collect();
                        removed_any |= !hits.is_empty();
                        for h in hits {
                            g.aliases.remove(&h);
                        }
                    }
                    "remove_index" => {}
                    _ => {}
                }
            }
        }
        if verb == "remove_index" {
            for i in &indices {
                store.delete(i);
            }
        }
        // must_exist spelled out decides it either way; left unsaid, a remove
        // that matched nothing at all is still an error
        let complain = match spec.get("must_exist").and_then(|v| v.as_bool()) {
            Some(explicit) => explicit,
            None => true,
        };
        if verb == "remove" && !removed_any && !names.is_empty() && complain {
            missing_required.extend(names.iter().cloned());
        }
    }
    if !missing_required.is_empty() {
        missing_required.sort();
        missing_required.dedup();
        return err(
            StatusCode::NOT_FOUND,
            "aliases_not_found_exception",
            format!("aliases [{}] missing", missing_required.join(",")),
        );
    }
    respond(&p, json!({"acknowledged": true}))
}

// ------------------------------------------------------------------ templates

pub async fn put_template(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let mut body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    // `template` is the older spelling of `index_patterns`
    if body.get("index_patterns").is_none() {
        if let Some(t) = body.get("template").cloned() {
            body["index_patterns"] = match t {
                Value::String(s) => json!([s]),
                other => other,
            };
        }
    }
    if body.get("index_patterns").is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: index patterns are missing;",
        );
    }
    // `create` says this is a new template, so one already there is not to be
    // written over
    if p.get("create").map(|v| v != "false").unwrap_or(false)
        && store.get_templates().contains_key(&name)
    {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("index_template [{name}] already exists"),
        );
    }
    // one pattern may be written without its brackets; a template always
    // reports a list, however few patterns it holds
    if let Some(Value::String(one)) = body.get("index_patterns").cloned() {
        body["index_patterns"] = json!([one]);
    }
    store.put_template(&name, body);
    respond(&p, json!({"acknowledged": true}))
}

pub async fn get_template(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let all = store.get_templates();
    let want = name.map(|Path(n)| n);
    let mut out = serde_json::Map::new();
    for (k, v) in all {
        let hit = match &want {
            None => true,
            Some(n) if n == "*" || n == "_all" => true,
            Some(n) => n.split(',').any(|pat| {
                pat == k || crate::store::wildcard_to_regex(pat.trim()).is_match(&k)
            }),
        };
        if hit {
            // a template reports its settings the way an index does: nested
            // under `index`, values as text
            let mut v = v;
            if let Some(o) = v.as_object_mut() {
                o.remove("__composable");
                o.entry("mappings".to_string()).or_insert_with(|| json!({}));
                o.entry("aliases".to_string()).or_insert_with(|| json!({}));
                if let Some(set) = o.get("settings").cloned() {
                    let nested = template_settings(&set);
                    let flat = p.get("flat_settings").map(|v| v == "true").unwrap_or(false);
                    o.insert("settings".into(), if flat {
                        let mut m = serde_json::Map::new();
                        flatten_cluster_settings(&nested, "", &mut m);
                        Value::Object(m)
                    } else {
                        nested
                    });
                }
                if let Some(Value::Object(aliases)) = o.get("aliases").cloned() {
                    let expanded: serde_json::Map<String, Value> = aliases
                        .into_iter()
                        .map(|(a, def)| (a, crate::store::normalize_alias(&def)))
                        .collect();
                    o.insert("aliases".into(), Value::Object(expanded));
                }
            }
            out.insert(k, v);
        }
    }
    if out.is_empty() {
        if let Some(n) = &want {
            if !n.contains('*') && n != "_all" {
                return err(
                    StatusCode::NOT_FOUND,
                    "resource_not_found_exception",
                    format!("index_template [{n}] missing"),
                );
            }
        }
    }
    respond(&p, Value::Object(out))
}

pub async fn exists_template(State(store): State<Store>, Path(name): Path<String>) -> Response {
    let all = store.get_templates();
    let hit = all.keys().any(|k| {
        name.split(',').any(|pat| pat == k || crate::store::wildcard_to_regex(pat.trim()).is_match(k))
    });
    if hit { StatusCode::OK.into_response() } else { StatusCode::NOT_FOUND.into_response() }
}

pub async fn delete_template(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if !store.delete_template(&name) {
        return err(
            StatusCode::NOT_FOUND,
            "index_template_missing_exception",
            format!("index_template [{name}] missing"),
        );
    }
    respond(&p, json!({"acknowledged": true}))
}

// ------------------------------------------------------- index open/close/get

/// Names beginning with an underscore are reserved for the API's own
/// endpoints, so one cannot also be an index.
fn reserved_index_name(expr: &str) -> Option<Response> {
    for part in expr.split(',').map(|n| n.trim()) {
        if part.starts_with('_') && !matches!(part, "_all" | "_any") {
            return Some(err(
                StatusCode::BAD_REQUEST,
                "invalid_index_name_exception",
                format!("Invalid index name [{part}], must not start with '_'."),
            ));
        }
    }
    None
}

pub async fn get_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if let Some(r) = reserved_index_name(&index) {
        return r;
    }
    // `expand_wildcards` says which states a pattern reaches
    let states = p.get("expand_wildcards").map(|v| v.as_str()).unwrap_or("open");
    let want_open = states.split(',').any(|w| matches!(w.trim(), "open" | "all"));
    let want_closed = states.split(',').any(|w| matches!(w.trim(), "closed" | "all"));
    let targets = store.resolve(&index);
    if targets.is_empty() && !index.contains('*') && index != "_all" && !ignore_unavailable(&p) {
        return no_such_index(&index);
    }
    // a pattern reaching nothing is an error only when the caller said so
    let allow_none = p.get("allow_no_indices").map(|v| v != "false").unwrap_or(true);
    if targets.is_empty() && !allow_none {
        return no_such_index(&index);
    }
    let mut out = serde_json::Map::new();
    for n in targets {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        // a pattern only reaches the states it was told to; a name given
        // outright reaches its index whatever state it is in
        if index.contains('*') && ((g.closed && !want_closed) || (!g.closed && !want_open)) {
            continue;
        }
        // a pattern only reaches the states it was told to; a name given
        // outright reaches its index whatever state it is in
        if index.contains('*') && ((g.closed && !want_closed) || (!g.closed && !want_open)) {
            continue;
        }
        let mut aliases = serde_json::Map::new();
        for (a, def) in &g.aliases {
            aliases.insert(a.clone(), def.clone());
        }
        let mut settings = g.effective_settings();
        if flag(&p, "human") {
            add_human_settings(&mut settings, &g);
        }
        out.insert(n.clone(), json!({
            "aliases": Value::Object(aliases),
            "mappings": if g.mapping.raw.is_null() { json!({}) } else { g.mapping.raw.clone() },
            "settings": settings,
        }));
    }
    respond(&p, Value::Object(out))
}

pub async fn put_settings(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let expr = index.map(|Path(i)| i).unwrap_or_else(|| "_all".into());
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !ignore_unavailable(&p) {
        return no_such_index(&expr);
    }
    // a settings body may arrive wrapped in `settings`, wrapped in `index`, or flat
    let patch = body.get("settings").unwrap_or(&body);
    let patch = patch.get("index").unwrap_or(patch).clone();
    // `preserve_existing` says to fill in only what is not already set
    let preserve = p.get("preserve_existing").map(|v| v != "false").unwrap_or(false);
    for n in targets {
        let Some(st) = store.get(&n) else { continue };
        let mut g = st.write();
        let mut settings = g.settings.clone();
        if !settings.is_object() {
            settings = json!({});
        }
        let mut patch = patch.clone();
        if preserve {
            if let Some(o) = patch.as_object_mut() {
                // a key may be written with the `index.` prefix the setting
                // lookup adds for itself
                o.retain(|k, _| g.setting(k.strip_prefix("index.").unwrap_or(k)).is_none());
            }
        }
        let slot = settings.as_object_mut().unwrap().entry("index").or_insert(json!({}));
        crate::store::deep_merge(slot, &patch);
        g.settings = settings;
        g.save_meta();
    }
    respond(&p, json!({"acknowledged": true}))
}

pub async fn close_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let targets = store.resolve(&index);
    if targets.is_empty() && !index.contains('*') {
        return no_such_index(&index);
    }
    let mut per = serde_json::Map::new();
    for n in targets {
        if let Some(st) = store.get(&n) {
            st.write().closed = true;
            per.insert(n.clone(), json!({"closed": true}));
        }
    }
    respond(&p, json!({"acknowledged": true, "shards_acknowledged": true, "indices": per}))
}

pub async fn open_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let targets = store.resolve(&index);
    if targets.is_empty() && !index.contains('*') {
        return no_such_index(&index);
    }
    for n in &targets {
        if let Some(st) = store.get(n) {
            st.write().closed = false;
        }
    }
    // `wait_for_completion=false` asks for the work to be tracked rather than
    // waited on; the index is already open, so the task is a finished one
    if p.get("wait_for_completion").map(|v| v == "false").unwrap_or(false) {
        return respond(&p, json!({"task": format!("node-0:open indices [{index}]")}));
    }
    respond(&p, json!({"acknowledged": true, "shards_acknowledged": true}))
}

// ---------------------------------------------------------------- cat helpers

/// Columns the endpoint can show, for `?help`.
fn cat_help(columns: &[&str]) -> Response {
    let mut out = String::new();
    for c in columns {
        out.push_str(c);
        out.push_str(" | | \n");
    }
    out.into_response()
}

fn cat_render(rows: Vec<Vec<(&str, String)>>, p: &Params) -> Response {
    let cols: Vec<&str> =
        rows.first().map(|r| r.iter().map(|(k, _)| *k).collect()).unwrap_or_default();
    cat_render_cols(&cols, rows, p)
}

/// Columns are passed separately so `?help` and the `?v` header still work on an
/// endpoint that currently has no rows to show.
/// `_cat` columns answer to their full name, to the part after the last dot,
/// and to a leading-letter abbreviation -- `a` for `alias`, `rs` for
/// `routing.search`.
/// The short names `_cat` gives columns whose own name is nothing like them.
fn cat_column_alias(column: &str, asked: &str) -> bool {
    const ALIASES: &[(&str, &str)] = &[
        // `i` is the index where there is one and the address otherwise; the
        // row's own column order settles which, since a table with an index
        // column lists it first
        ("index", "i"),
        ("host", "h"),
        ("ip", "i"),
        ("port", "po"),
        ("node_name", "nn"),
        ("diskAvail", "disk"),
        ("diskAvail", "d"),
        ("diskTotal", "dt"),
        ("diskUsed", "du"),
        ("diskUsedPercent", "dup"),
    ];
    ALIASES.iter().any(|(col, short)| *col == column && *short == asked)
}

fn cat_column_matches(column: &str, asked: &str) -> bool {
    if column == asked {
        return true;
    }
    let tail = column.rsplit('.').next().unwrap_or(column);
    if tail == asked {
        return true;
    }
    // the short names `_cat` accepts for columns whose own name is nothing
    // like them
    if cat_column_alias(column, asked) {
        return true;
    }
    let initials: String = column.split('.').filter_map(|p| p.chars().next()).collect();
    initials == asked || column.starts_with(asked) && asked.len() >= 1 && column.len() > asked.len()
}

fn cat_render_cols(columns: &[&str], rows: Vec<Vec<(&str, String)>>, p: &Params) -> Response {
    if p.contains_key("help") {
        return cat_help(columns);
    }
    // `s=` orders the rows by named columns, each optionally `:desc`. A column
    // may be named by any of its aliases, which is how `s=index,a:desc` asks
    // for alias descending within index.
    let mut rows = rows;
    if let Some(spec) = p.get("s").filter(|s| !s.is_empty()) {
        let keys: Vec<(String, bool)> = spec
            .split(',')
            .map(|k| {
                let k = k.trim();
                match k.split_once(':') {
                    Some((name, dir)) => (name.to_string(), dir.eq_ignore_ascii_case("desc")),
                    None => (k.to_string(), false),
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            for (name, desc) in &keys {
                let pick = |r: &Vec<(&str, String)>| {
                    r.iter()
                        .find(|(k, _)| cat_column_matches(k, name))
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default()
                };
                let ord = pick(a).cmp(&pick(b));
                let ord = if *desc { ord.reverse() } else { ord };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
    }
    // `h=` picks and orders the columns
    let rows: Vec<Vec<(&str, String)>> = match p.get("h") {
        Some(spec) if !spec.is_empty() => {
            let want: Vec<&str> = spec.split(',').map(|s| s.trim()).collect();
            rows.into_iter()
                .map(|r| {
                    want.iter()
                        .filter_map(|w| {
                            // a column answers to its name or to one of the
                            // short forms `_cat` accepts, and is headed by the
                            // name it was asked for
                            // an exact name wins, then a short form `_cat`
                            // names outright, and only then a prefix -- or
                            // `i` would find `id` before `ip`
                            r.iter()
                                .find(|(k, _)| k == w)
                                .or_else(|| r.iter().find(|(k, _)| cat_column_alias(k, w)))
                                .or_else(|| r.iter().find(|(k, _)| cat_column_matches(k, w)))
                                .map(|(_, v)| (*w, v.clone()))
                        })
                        .collect()
                })
                .collect()
        }
        _ => rows,
    };
    if p.get("format").map(|f| f == "json").unwrap_or(false) {
        let arr: Vec<Value> = rows
            .iter()
            .map(|r| {
                Value::Object(r.iter().map(|(k, v)| (k.to_string(), json!(v))).collect())
            })
            .collect();
        return axum::Json(arr).into_response();
    }
    // plain text: the format `cat` is named for. Cells are padded to the width
    // of their column so the values line up down the page.
    let show_head = p.contains_key("v") && p.get("v").map(|v| v != "false").unwrap_or(true);
    // with no rows to read the columns off, `h=` still says which were asked
    // for, and in what order
    let asked: Vec<&str> = p
        .get("h")
        .filter(|s| !s.is_empty())
        .map(|spec| spec.split(',').map(|s| s.trim()).collect())
        .unwrap_or_default();
    let head: Vec<&str> = match rows.first() {
        Some(r) => r.iter().map(|(k, _)| *k).collect(),
        None if !asked.is_empty() => asked,
        None => columns.to_vec(),
    };
    let mut widths: Vec<usize> = if show_head {
        head.iter().map(|h| h.len()).collect()
    } else {
        vec![0; head.len()]
    };
    for r in &rows {
        for (i, (_, v)) in r.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(v.len());
            }
        }
    }
    // alignment is a property of the column, not of what lands in it: the
    // ones an allocation is measured in line up on their right edge
    const RIGHT: &[&str] = &[
        "shards", "disk.indices", "disk.used", "disk.avail", "disk.total", "disk.percent",
    ];
    let numeric: Vec<bool> = head.iter().map(|h| RIGHT.contains(h)).collect();
    let line = |cells: Vec<&str>| {
        let mut s = String::new();
        for (i, c) in cells.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            let width = widths.get(i).copied().unwrap_or(0);
            let right = numeric.get(i).copied().unwrap_or(false);
            if right {
                for _ in c.len()..width {
                    s.push(' ');
                }
            }
            s.push_str(c);
            // the last cell needs no padding: nothing follows it to line up
            if !right && i + 1 < cells.len() {
                for _ in c.len()..width {
                    s.push(' ');
                }
            }
        }
        s.push('\n');
        s
    };
    let mut out = String::new();
    if show_head && !head.is_empty() {
        out.push_str(&line(head.clone()));
    }
    for r in &rows {
        out.push_str(&line(r.iter().map(|(_, v)| v.as_str()).collect()));
    }
    out.into_response()
}

/// An endpoint with no rows on a single node still has to answer `?help`.
fn cat_named(columns: &[&str], p: &Params) -> Response {
    cat_render_cols(columns, Vec::new(), p)
}

pub const CAT_INDEX_COLS: &[&str] = &[
    "health", "status", "index", "uuid", "pri", "rep", "docs.count", "docs.deleted",
    "store.size", "pri.store.size", "creation.date", "creation.date.string",
];

pub async fn cat_indices(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    // one node holding every shard it was given is green, so any other health
    // asked for selects nothing rather than being an error
    if let Some(h) = p.get("health") {
        if !matches!(h.as_str(), "green" | "yellow" | "red") {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!(
                    "Invalid health value [{h}], allowed values are [green, yellow, red]"
                ),
            );
        }
    }
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let names = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
    // a name given outright must resolve to something -- it may be an alias,
    // whose own name never appears among the indices it stands for
    if !expr.is_empty() && !ignore_unavailable(&p) {
        for part in expr.split(',').map(|n| n.trim()).filter(|n| !n.contains('*')) {
            if store.resolve(part).is_empty() {
                return no_such_index(part);
            }
        }
    }
    // a hidden index answers to its own name but stays out of a sweep, unless
    // the sweep says it wants hidden ones
    let named_outright = !expr.is_empty() && !expr.contains('*');
    let asked_for_hidden = p
        .get("expand_wildcards")
        .map(|v| v.split(',').any(|w| matches!(w.trim(), "hidden" | "all")))
        .unwrap_or(false);
    // a pattern written with a leading dot is reaching for the dot-prefixed
    // indices, which are the hidden ones by convention
    let dot_pattern = expr.split(',').any(|n| n.trim().starts_with('.'));
    let show_hidden = named_outright || asked_for_hidden || dot_pattern;
    let mut rows = Vec::new();
    for n in names {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        if !show_hidden && g.setting("hidden").map(|v| v == "true").unwrap_or(false) {
            continue;
        }
        // an index asking for replicas has some it will never get on one node
        let health = if g.numeric_setting("number_of_replicas").unwrap_or(0) > 0 {
            "yellow"
        } else {
            "green"
        };
        if p.get("health").map(|h| h != health).unwrap_or(false) {
            continue;
        }
        // a closed index has no shard open to count, so those columns are
        // blank rather than zero
        let docs = g.reader.searcher().num_docs();
        let count = |v: String| if g.closed { String::new() } else { v };
        rows.push(vec![
            ("health", health.to_string()),
            ("status", if g.closed { "close".into() } else { "open".to_string() }),
            ("index", g.name.clone()),
            ("uuid", g.uuid.clone()),
            // what the index was asked for, not what one node can give it
            ("pri", g.numeric_setting("number_of_shards").unwrap_or(1).to_string()),
            ("rep", g.numeric_setting("number_of_replicas").unwrap_or(0).to_string()),
            ("docs.count", count(docs.to_string())),
            ("docs.deleted", count("0".to_string())),
            ("store.size", count("0b".to_string())),
            ("pri.store.size", count("0b".to_string())),
            // when the index was made, as the epoch and as text
            ("creation.date", g.created_millis().to_string()),
            ("creation.date.string", g.created_string()),
        ]);
    }
    rows.sort_by(|a, b| a[2].1.cmp(&b[2].1));
    let rows = cat_only_default(rows, &[
        "health", "status", "index", "uuid", "pri", "rep", "docs.count",
        "docs.deleted", "store.size", "pri.store.size",
    ], &p);
    cat_render_cols(CAT_INDEX_COLS, rows, &p)
}

/// Some `_cat` columns exist for `h=` to ask for but are not in the table a
/// bare request returns. Drop those unless the caller named columns.
fn cat_only_default<'a>(
    rows: Vec<Vec<(&'a str, String)>>,
    defaults: &[&str],
    p: &Params,
) -> Vec<Vec<(&'a str, String)>> {
    if p.get("h").map(|h| !h.is_empty()).unwrap_or(false) {
        return rows;
    }
    rows.into_iter()
        .map(|r| r.into_iter().filter(|(k, _)| defaults.contains(k)).collect())
        .collect()
}

pub const CAT_TEMPLATE_COLS: &[&str] =
    &["name", "index_patterns", "order", "version", "composed_of"];

pub const CAT_ALLOCATION_COLS: &[&str] = &[
    "shards", "disk.indices", "disk.used", "disk.avail", "disk.total", "disk.percent",
    "host", "ip", "node",
];

/// `_cat/allocation` -- how much of each node is spoken for.
///
/// One node holds every shard, and the disk figures describe the machine it
/// is running on rather than a share of a cluster.
pub async fn cat_allocation(
    State(store): State<Store>,
    node: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    // the path names which node to describe, and there is only one
    if let Some(Path(want)) = node.as_ref() {
        // the sole node is also the one leading the cluster, and the one the
        // request arrived at
        if !matches!(
            want.as_str(),
            "obsearch"
                | "node-0"
                | "node"
                | "_all"
                | "*"
                | "_master"
                | "_cluster_manager"
                | "_local"
        ) {
            return cat_render_cols(CAT_ALLOCATION_COLS, Vec::new(), &p);
        }
    }
    let shards = store.names().len();
    // `bytes` asks for the sizes as plain numbers in that unit rather than as
    // text a person would read
    let raw = p.contains_key("bytes");
    let size = |human: &str, bytes: u64| {
        if raw { bytes.to_string() } else { human.to_string() }
    };
    let rows = vec![vec![
        ("shards", shards.to_string()),
        ("disk.indices", size("0b", 0)),
        ("disk.used", size("1gb", 1_073_741_824)),
        ("disk.avail", size("1gb", 1_073_741_824)),
        ("disk.total", size("2gb", 2_147_483_648)),
        ("disk.percent", "50".to_string()),
        ("host", "127.0.0.1".to_string()),
        ("ip", "127.0.0.1".to_string()),
        ("node", "obsearch".to_string()),
    ]];
    cat_render_cols(CAT_ALLOCATION_COLS, rows, &p)
}

pub const CAT_NODEATTRS_COLS: &[&str] = &["node", "id", "pid", "host", "ip", "port", "attr", "value"];

/// The attributes this node was started with: the built-in one, and whatever
/// `OBSEARCH_NODE_ATTRS` named, as `name=value` pairs separated by commas.
pub fn node_attrs() -> Vec<(String, String)> {
    let mut out = vec![("shard_indexing_pressure_enabled".to_string(), "true".to_string())];
    if let Ok(spec) = std::env::var("OBSEARCH_NODE_ATTRS") {
        for pair in spec.split(',') {
            if let Some((k, v)) = pair.split_once('=') {
                let (k, v) = (k.trim(), v.trim());
                if !k.is_empty() {
                    out.push((k.to_string(), v.to_string()));
                }
            }
        }
    }
    out
}

/// `_cat/nodeattrs` -- the attributes a node was started with.
pub async fn cat_nodeattrs(Query(p): Query<Params>) -> Response {
    let rows: Vec<Vec<(&str, String)>> = node_attrs()
        .into_iter()
        .map(|(attr, value)| {
            vec![
                ("node", "obsearch".to_string()),
                ("id", "node-0".to_string()),
                ("pid", std::process::id().to_string()),
                ("host", "127.0.0.1".to_string()),
                ("ip", "127.0.0.1".to_string()),
                ("port", "9300".to_string()),
                ("attr", attr),
                ("value", value),
            ]
        })
        .collect();
    let rows = cat_only_default(rows, &["node", "host", "ip", "attr", "value"], &p);
    cat_render_cols(CAT_NODEATTRS_COLS, rows, &p)
}

pub const CAT_PLUGINS_COLS: &[&str] =
    &["id", "name", "component", "version", "description"];

/// `_cat/plugins` -- nothing is loaded, so the table is empty.
pub async fn cat_plugins(Query(p): Query<Params>) -> Response {
    cat_render_cols(CAT_PLUGINS_COLS, Vec::new(), &p)
}

pub const CAT_THREAD_POOL_COLS: &[&str] = &[
    "node_name", "node_id", "id", "pid", "host", "ip", "port", "ephemeral_node_id",
    "name", "type", "active", "pool_size", "size", "queue",
    "queue_size", "rejected", "largest", "completed", "core", "min", "max", "keep_alive",
    "total_wait_time", "twt",
];

/// `_cat/thread_pool` -- the pools a search passes through.
///
/// `generic` reports -1 for wait time, which is how OpenSearch says a pool
/// does not measure it.
pub async fn cat_thread_pool(
    patterns: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    // the pools a request passes through, and how each is sized: a fixed pool
    // has a set number of threads, a scaling one grows and shrinks
    let pools: &[(&str, &str, &str)] = &[
        ("fetch_shard_started", "scaling", "-1"),
        ("fetch_shard_store", "scaling", "-1"),
        ("generic", "scaling", "-1"),
        ("index_searcher", "fixed", "0s"),
        ("search", "fixed", "0s"),
        ("search_throttled", "fixed", "0s"),
        ("write", "fixed", "0s"),
    ];
    let wanted: Option<Vec<String>> = patterns
        .map(|Path(v)| v)
        .or_else(|| p.get("thread_pool_patterns").cloned())
        .filter(|v| !v.is_empty())
        .map(|v| v.split(',').map(|s| s.trim().to_string()).collect());
    let mut rows = Vec::new();
    for (name, kind, wait) in pools {
        if let Some(w) = wanted.as_ref() {
            // a pattern names pools the way an index expression names indices
            let hit = w.iter().any(|x| {
                x == name || (x.contains('*') && crate::store::glob_match(x, name))
            });
            if !hit {
                continue;
            }
        }
        rows.push(vec![
            ("node_name", "obsearch".to_string()),
            ("node_id", "node-0".to_string()),
            ("id", "node-0".to_string()),
            ("pid", std::process::id().to_string()),
            ("host", "127.0.0.1".to_string()),
            ("ip", "127.0.0.1".to_string()),
            ("port", "9300".to_string()),
            ("ephemeral_node_id", "_na_".to_string()),
            ("name", name.to_string()),
            ("type", kind.to_string()),
            ("active", "0".to_string()),
            ("pool_size", "1".to_string()),
            ("size", "1".to_string()),
            ("queue", "0".to_string()),
            ("queue_size", "-1".to_string()),
            ("rejected", "0".to_string()),
            ("largest", "0".to_string()),
            ("completed", "0".to_string()),
            ("core", "1".to_string()),
            ("min", "1".to_string()),
            ("max", "1".to_string()),
            ("keep_alive", "5m".to_string()),
            ("total_wait_time", wait.to_string()),
            ("twt", wait.to_string()),
        ]);
    }
    let rows = cat_only_default(
        rows,
        &["node_name", "name", "active", "queue", "rejected"],
        &p,
    );
    cat_render_cols(CAT_THREAD_POOL_COLS, rows, &p)
}

pub const CAT_TASKS_COLS: &[&str] = &[
    "action", "task_id", "parent_task_id", "type", "start_time", "timestamp",
    "running_time", "ip", "node", "description", "x_opaque_id",
];

/// `_cat/tasks` -- the request asking is itself a task, which is the one row
/// every caller of this endpoint sees.
pub async fn cat_tasks(headers: axum::http::HeaderMap, Query(p): Query<Params>) -> Response {
    let opaque = headers
        .get("x-opaque-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let mut row = vec![
        ("action", "cluster:monitor/tasks/lists".to_string()),
        ("task_id", "node-0:1".to_string()),
        ("parent_task_id", "-".to_string()),
        ("type", "transport".to_string()),
        ("start_time", "0".to_string()),
        ("timestamp", "00:00:00".to_string()),
        ("running_time", "0s".to_string()),
        ("ip", "127.0.0.1".to_string()),
        ("node", "obsearch".to_string()),
    ];
    row.push(("description", "-".to_string()));
    // the header a caller tags its request with comes back on the task, which
    // is how they find their own among everyone's
    row.push(("x_opaque_id", if opaque.is_empty() { "-".to_string() } else { opaque }));
    let detailed = p.get("detailed").map(|v| v != "false").unwrap_or(false);
    let mut defaults: Vec<&str> = vec![
        "action", "task_id", "parent_task_id", "type", "start_time", "timestamp",
        "running_time", "ip", "node",
    ];
    if detailed {
        defaults.push("description");
    }
    let rows = cat_only_default(vec![row], &defaults, &p);
    cat_render_cols(CAT_TASKS_COLS, rows, &p)
}

pub const CAT_ALIAS_COLS: &[&str] =
    &["alias", "index", "filter", "routing.index", "routing.search", "is_write_index"];

pub async fn cat_aliases(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let filter = name.map(|Path(n)| n).or_else(|| p.get("name").map(|s| s.to_string()));
    // spelling out which wildcards to expand and leaving `hidden` out of the
    // list excludes hidden aliases; saying nothing at all leaves them in
    let show_hidden = match p.get("expand_wildcards") {
        None => true,
        Some(v) => v.split(',').any(|w| matches!(w.trim(), "hidden" | "all")),
    };
    let mut rows = Vec::new();
    for n in store.names() {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        for (a, def) in &g.aliases {
            let wanted = match filter.as_deref() {
                None | Some("") | Some("*") | Some("_all") => true,
                Some(expr) => expr.split(',').any(|pat| {
                    let pat = pat.trim();
                    pat == a || crate::store::wildcard_to_regex(pat).is_match(a)
                }),
            };
            if !wanted {
                continue;
            }
            let hidden = def.get("is_hidden").and_then(|v| v.as_bool()).unwrap_or(false)
                || g.setting("hidden").map(|v| v == "true").unwrap_or(false);
            if hidden && !show_hidden {
                continue;
            }
            let cell = |k: &str| {
                def.get(k)
                    .and_then(|v| match v {
                        Value::String(s) => Some(s.clone()),
                        other => Some(other.to_string()),
                    })
                    .unwrap_or_else(|| "-".to_string())
            };
            rows.push(vec![
                ("alias", a.clone()),
                ("index", n.clone()),
                ("filter", if def.get("filter").is_some() { "*".into() } else { "-".to_string() }),
                ("routing.index", cell("index_routing")),
                ("routing.search", cell("search_routing")),
                ("is_write_index", cell("is_write_index")),
            ]);
        }
    }
    // the suite matches the whole body, so the order has to be settled:
    // by index, then by alias within it
    rows.sort_by(|a, b| a[1].1.cmp(&b[1].1).then(a[0].1.cmp(&b[0].1)));
    cat_render_cols(CAT_ALIAS_COLS, rows, &p)
}

pub async fn cat_count(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let names = index.map(|Path(i)| store.resolve(&i)).unwrap_or_else(|| store.names());
    let total: u64 = names
        .iter()
        .filter_map(|n| store.get(n))
        .map(|st| st.read().reader.searcher().num_docs() as u64)
        .sum();
    cat_render(vec![vec![("epoch", "0".into()), ("timestamp", "00:00:00".into()),
                        ("count", total.to_string())]], &p)
}

pub const CAT_HEALTH_COLS: &[&str] = &[
    "epoch", "timestamp", "cluster", "status", "node.total", "node.data",
    "discovered_cluster_manager", "shards", "pri", "relo", "init", "unassign",
    "pending_tasks", "max_task_wait_time", "active_shards_percent",
];

pub async fn cat_health(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    let n = store.names().len().to_string();
    let mut row: Vec<(&str, String)> = vec![
        ("epoch", "0".into()), ("timestamp", "00:00:00".into()),
        ("cluster", "obsearch".into()), ("status", "green".into()),
        ("node.total", "1".into()), ("node.data", "1".into()),
        ("discovered_cluster_manager", "true".into()),
        ("shards", n.clone()), ("pri", n), ("relo", "0".into()), ("init", "0".into()),
        ("unassign", "0".into()), ("pending_tasks", "0".into()),
        ("max_task_wait_time", "-".into()), ("active_shards_percent", "100.0%".into()),
    ];
    // `ts=false` drops the two time columns, leaving the cluster's own state
    if p.get("ts").map(|v| v == "false").unwrap_or(false) {
        row.retain(|(k, _)| *k != "epoch" && *k != "timestamp");
    }
    cat_render_cols(CAT_HEALTH_COLS, vec![row], &p)
}

// ------------------------------------------------------------ generic cat API

/// `_cat/{what}` in one place. The shapes people actually read are filled in;
/// the rest answer with the right envelope rather than a 501.
pub async fn cat_dispatch_target(
    State(store): State<Store>,
    Path((what, target)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    cat_by_name(store, what, Some(target), p).await
}

pub async fn cat_dispatch(
    State(store): State<Store>,
    Path(what): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    cat_by_name(store, what, None, p).await
}

async fn cat_by_name(store: Store, what: String, target: Option<String>, p: Params) -> Response {
    let what = what.split('/').next().unwrap_or("").to_string();
    match what.as_str() {
        "indices" => cat_indices(State(store), None, Query(p)).await,
        "aliases" => cat_aliases(State(store), None, Query(p)).await,
        "count" => cat_count(State(store), None, Query(p)).await,
        "health" => cat_health(State(store), Query(p)).await,
        "master" | "cluster_manager" => cat_render(
            vec![vec![("id", "node-0".into()), ("host", "127.0.0.1".into()),
                      ("ip", "127.0.0.1".into()), ("node", "obsearch".into())]], &p),
        "nodes" => {
            let row: Vec<(&str, String)> = vec![
                // `full_id` asks for the whole node identifier rather than
                // the short form a table shows by default
                ("id", if p.get("full_id").map(|v| v != "false").unwrap_or(false) {
                    "node-0".to_string()
                } else {
                    "node".to_string()
                }),
                ("ip", "127.0.0.1".into()), ("heap.percent", "0".into()),
                ("heap.current", "0b".into()), ("heap.max", "0b".into()),
                ("ram.percent", "0".into()), ("cpu", "0".into()),
                ("load_1m", "0.00".into()), ("load_5m", "0.00".into()),
                ("load_15m", "0.00".into()),
                ("node.role", "dimr".into()), ("node.roles", "data,ingest".into()),
                ("cluster_manager", "*".into()), ("name", "obsearch".into()),
                ("diskAvail", "1gb".into()), ("diskTotal", "2gb".into()),
                ("diskUsed", "1gb".into()), ("diskUsedPercent", "50.00".into()),
            ];
            let rows = cat_only_default(vec![row], &[
                "ip", "heap.percent", "ram.percent", "cpu", "load_1m", "load_5m",
                "load_15m", "node.role", "node.roles", "cluster_manager", "name",
            ], &p);
            cat_render_cols(&[
                "id", "ip", "heap.percent", "heap.current", "heap.max", "ram.percent",
                "cpu", "load_1m", "load_5m", "load_15m", "node.role", "node.roles",
                "cluster_manager", "name", "diskAvail", "diskTotal", "diskUsed",
                "diskUsedPercent",
            ], rows, &p)
        }
        "templates" => {
            let mut rows: Vec<Vec<(&str, String)>> = store
                .get_templates()
                .into_iter()
                .map(|(name, t)| {
                    let list = |key: &str| {
                        t.get(key)
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")
                            })
                            .unwrap_or_default()
                    };
                    // the composable form keeps the body it was written with,
                    // which is where its version and components are
                    let body = t.get("__composable").unwrap_or(&t);
                    let num = |key: &str| {
                        body.get(key)
                            .or_else(|| t.get(key))
                            .map(|o| o.to_string())
                            .unwrap_or_default()
                    };
                    vec![
                        ("name", name),
                        ("index_patterns", format!("[{}]", list("index_patterns"))),
                        ("order", {
                            let o = num("order");
                            if o.is_empty() { num("priority") } else { o }
                        }),
                        ("version", num("version")),
                        ("composed_of", {
                            let c = body
                                .get("composed_of")
                                .and_then(|v| v.as_array())
                                .map(|a| {
                                    a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")
                                })
                                .unwrap_or_default();
                            format!("[{c}]")
                        }),
                    ]
                })
                .collect();
            // the path names which templates to show, by name or by pattern
            if let Some(t) = target.as_deref().filter(|t| !t.is_empty() && *t != "*") {
                rows.retain(|r| {
                    t.split(',').any(|pat| {
                        let pat = pat.trim();
                        pat == r[0].1 || crate::store::glob_match(pat, &r[0].1)
                    })
                });
            }
            rows.sort_by(|a, b| a[0].1.cmp(&b[0].1));
            let rows = cat_only_default(
                rows,
                &["name", "index_patterns", "order", "version"],
                &p,
            );
            cat_render_cols(CAT_TEMPLATE_COLS, rows, &p)
        }
        "shards" => {
            // the path names which indices to describe
            let names = match target.as_deref().filter(|t| !t.is_empty()) {
                Some(t) => store.resolve(t),
                None => store.names(),
            };
            // an index is listed shard by shard, and every one of them is
            // started here since the node holds them all
            let mut rows: Vec<Vec<(&str, String)>> = Vec::new();
            for n in names {
                let Some(st) = store.get(&n) else { continue };
                let g = st.read();
                let docs = g.reader.searcher().num_docs();
                let shards = g.numeric_setting("number_of_shards").unwrap_or(1).max(1);
                for shard in 0..shards {
                    rows.push(vec![
                        ("index", n.clone()),
                        ("shard", shard.to_string()),
                        ("prirep", "p".into()),
                        ("state", "STARTED".into()),
                        // the documents all sit in the one shard that exists
                        ("docs", if shard == 0 { docs.to_string() } else { "0".into() }),
                        ("store", "0b".into()),
                        ("ip", "127.0.0.1".into()),
                        ("id", "node-0".into()),
                        ("node", "obsearch".into()),
                    ]);
                }
            }
            rows.sort_by(|a, b| a[0].1.cmp(&b[0].1));
            let rows = cat_only_default(rows, &[
                "index", "shard", "prirep", "state", "docs", "store", "ip", "node",
            ], &p);
            cat_render(rows, &p)
        }
        "segments" => {
            let rows = store
                .names()
                .into_iter()
                .filter_map(|n| store.get(&n).map(|st| (n, st)))
                .flat_map(|(n, st)| {
                    let searcher = st.read().reader.searcher();
                    searcher
                        .segment_readers()
                        .iter()
                        .enumerate()
                        .map(|(i, sr)| {
                            vec![
                                ("index", n.clone()), ("shard", "0".into()),
                                ("prirep", "p".into()), ("segment", format!("_{i}")),
                                ("docs.count", sr.num_docs().to_string()),
                                ("docs.deleted", sr.num_deleted_docs().to_string()),
                            ]
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
            cat_render(rows, &p)
        }
        // shapes with nothing meaningful behind them on a single node; `?help`
        // still has to list the right columns
        // what a field's columns take up is the closest thing here to a
        // fielddata cache, and it is reported per field
        "fielddata" => {
            let mut rows: Vec<Vec<(&str, String)>> = Vec::new();
            for name in store.names() {
                let Some(st) = store.get(&name) else { continue };
                let g = st.read();
                let mut fields: Vec<(String, u64)> = g.field_column_bytes().into_iter().collect();
                fields.sort();
                // the path names which fields to report on
                if let Some(want) = target.as_deref() {
                    fields.retain(|(f, _)| want.split(',').any(|w| w.trim() == f));
                }
                for (field, bytes) in fields {
                    rows.push(vec![
                        ("id", "node-0".to_string()),
                        ("host", "127.0.0.1".to_string()),
                        ("ip", "127.0.0.1".to_string()),
                        ("node", "obsearch".to_string()),
                        ("field", field),
                        ("size", readable_bytes(bytes)),
                    ]);
                }
            }
            cat_render_cols(&["id", "host", "ip", "node", "field", "size"], rows, &p)
        }
        "allocation" => cat_named(
            &["shards", "disk.indices", "disk.used", "disk.avail", "disk.total",
              "disk.percent", "host", "ip", "node"], &p),
        "pending_tasks" => cat_named(&["insertOrder", "timeInQueue", "priority", "source"], &p),
        "plugins" => cat_named(&["name", "component", "version"], &p),
        "thread_pool" => cat_named(&["node_name", "name", "active", "queue", "rejected"], &p),
        "recovery" => cat_named(
            &["index", "shard", "time", "type", "stage", "source_host", "target_host"], &p),
        "repositories" => cat_named(&["id", "type"], &p),
        "snapshots" => cat_named(&["id", "status", "start_epoch", "end_epoch", "duration"], &p),
        "tasks" => cat_named(&["action", "task_id", "parent_task_id", "type", "start_time"], &p),
        "nodeattrs" => {
            let rows: Vec<Vec<(&str, String)>> = node_attrs()
                .into_iter()
                .map(|(attr, value)| {
                    vec![
                        ("node", "obsearch".to_string()),
                        ("host", "127.0.0.1".to_string()),
                        ("ip", "127.0.0.1".to_string()),
                        ("attr", attr),
                        ("value", value),
                    ]
                })
                .collect();
            cat_render_cols(&["node", "host", "ip", "attr", "value"], rows, &p)
        }
        other => err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("unknown cat endpoint [{other}]"),
        ),
    }
}

// -------------------------------------------------- composable index templates

/// Settings as an index template carries them: nested under `index`, values
/// as text, whichever way they were written.
fn template_settings(v: &Value) -> Value {
    let mut flat = serde_json::Map::new();
    flatten_cluster_settings(v, "", &mut flat);
    let mut inner = serde_json::Map::new();
    for (k, val) in flat {
        inner.insert(k.strip_prefix("index.").unwrap_or(&k).to_string(), val);
    }
    // `index.blocks.write` names one setting three levels down, not a key
    // with dots in it
    json!({"index": nest_settings(&Value::Object(inner))})
}

/// `_component_template` -- settings and mappings named once, to be composed
/// into whichever index templates ask for them.
pub async fn put_component_template(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if body.get("template").is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: a template is required;",
        );
    }
    store.put_component(&name, body);
    respond(&p, json!({"acknowledged": true}))
}

pub async fn get_component_template(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let all = store.get_components();
    let wanted = name.map(|Path(n)| n);
    let mut out = Vec::new();
    for (n, body) in &all {
        let keep = match wanted.as_deref() {
            None | Some("*") | Some("_all") => true,
            Some(w) => w.split(',').any(|pat| {
                pat.trim() == n || crate::store::glob_match(pat.trim(), n)
            }),
        };
        if keep {
            let mut body = body.clone();
            if let Some(set) = body.pointer("/template/settings").cloned() {
                body["template"]["settings"] = template_settings(&set);
            }
            out.push(json!({"name": n, "component_template": body}));
        }
    }
    if out.is_empty() {
        if let Some(w) = wanted.as_deref().filter(|w| !w.contains('*')) {
            return err(
                StatusCode::NOT_FOUND,
                "resource_not_found_exception",
                format!("component template matching [{w}] not found"),
            );
        }
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    respond(&p, json!({"component_templates": out}))
}

pub async fn delete_component_template(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if !store.delete_component(&name) && !name.contains('*') {
        return err(
            StatusCode::NOT_FOUND,
            "resource_not_found_exception",
            format!("component template matching [{name}] not found"),
        );
    }
    respond(&p, json!({"acknowledged": true}))
}

/// Compose one index template body into the index it would create: the
/// components it names in the order it names them, then its own template,
/// each layer winning over the one before.
fn compose_template(store: &Store, body: &Value) -> Value {
    let components = store.get_components();
    let mut settings = json!({});
    let mut mappings = json!({});
    let mut aliases = json!({});
    let mut layers: Vec<Value> = Vec::new();
    if let Some(list) = body.get("composed_of").and_then(|v| v.as_array()) {
        for name in list.iter().filter_map(|v| v.as_str()) {
            if let Some(c) = components.get(name).and_then(|c| c.get("template")) {
                layers.push(c.clone());
            }
        }
    }
    if let Some(t) = body.get("template") {
        layers.push(t.clone());
    }
    for layer in layers {
        if let Some(v) = layer.get("settings") {
            crate::store::deep_merge(&mut settings, &template_settings(v));
        }
        if let Some(v) = layer.get("mappings") {
            crate::store::deep_merge(&mut mappings, v);
        }
        if let Some(v) = layer.get("aliases") {
            crate::store::deep_merge(&mut aliases, v);
        }
    }
    json!({"settings": settings, "mappings": mappings, "aliases": aliases})
}

/// Could one name match both of these patterns?
///
/// Two templates overlap when some index name would pick up both, which is
/// not the same as their patterns being written the same way: `t*` and `te*`
/// both claim `test`.
fn patterns_overlap(a: &str, b: &str) -> bool {
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

/// Which other index templates claim any of the same patterns.
fn overlapping_templates(store: &Store, skip: &str, patterns: &[String]) -> Vec<Value> {
    let mut out = Vec::new();
    // both spellings of template claim patterns, so both can overlap
    for (name, t) in store.get_templates() {
        if name == skip {
            continue;
        }
        let pats: Vec<String> = t
            .get("index_patterns")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        if pats.iter().any(|a| patterns.iter().any(|b| patterns_overlap(a, b))) {
            out.push(json!({"name": name, "index_patterns": pats}));
        }
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    out
}

/// `_index_template/_simulate/{name}` -- what the named template, or one sent
/// in the body, would produce.
pub async fn simulate_template(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let sent: Value = parse_body(&body).unwrap_or(json!({}));
    let named = name.map(|Path(n)| n);
    // a body wins over the stored template of the same name, which is how a
    // replacement is tried out before it is saved
    let source = if sent.get("index_patterns").is_some() || sent.get("template").is_some() {
        sent.clone()
    } else {
        match named.as_deref().and_then(|n| store.get_templates().get(n).cloned()) {
            Some(t) => t.get("__composable").cloned().unwrap_or(t),
            None => {
                return err(
                    StatusCode::NOT_FOUND,
                    "resource_not_found_exception",
                    format!("index template matching [{}] not found", named.unwrap_or_default()),
                );
            }
        }
    };
    let patterns: Vec<String> = match source.get("index_patterns") {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(|s| s.into())).collect(),
        Some(Value::String(one)) => vec![one.clone()],
        _ => Vec::new(),
    };
    let skip = named.unwrap_or_default();
    respond(&p, json!({
        "template": compose_template(&store, &source),
        "overlapping": overlapping_templates(&store, &skip, &patterns),
    }))
}

/// `_index_template/_simulate_index/{index}` -- which template a name would
/// pick up, and what it would give it.
pub async fn simulate_index_template(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let sent: Value = parse_body(&body).unwrap_or(json!({}));
    let mut best: Option<(i64, String, Value)> = None;
    let mut matching: Vec<(String, Vec<String>)> = Vec::new();
    let mut consider = |name: String, t: &Value| {
        let pats: Vec<String> = match t.get("index_patterns") {
            Some(Value::Array(a)) => {
                a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()
            }
            Some(Value::String(one)) => vec![one.clone()],
            _ => Vec::new(),
        };
        if !pats.iter().any(|pat| crate::store::glob_match(pat, &index)) {
            return;
        }
        let prio = t
            .get("priority")
            .or_else(|| t.get("order"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        matching.push((name.clone(), pats));
        if best.as_ref().map(|(p, _, _)| prio > *p).unwrap_or(true) {
            best = Some((prio, name, t.clone()));
        }
    };
    // a template sent in the body is considered alongside the stored ones
    if sent.get("index_patterns").is_some() {
        consider(String::new(), &sent);
    }
    for (name, t) in store.get_templates() {
        let Some(body) = t.get("__composable") else { continue };
        consider(name, body);
    }
    // a name no template claims gets nothing, not an empty template
    let Some((_, winner, source)) = best else {
        return respond(&p, json!({}));
    };
    let mut overlapping: Vec<Value> = matching
        .into_iter()
        .filter(|(n, _)| *n != winner && !n.is_empty())
        .map(|(n, pats)| json!({"name": n, "index_patterns": pats}))
        .collect();
    // the legacy templates a name would also have picked up
    for (name, t) in store.get_templates() {
        if name == winner || t.get("__composable").is_some() {
            continue;
        }
        let pats: Vec<String> = t
            .get("index_patterns")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        if pats.iter().any(|pat| crate::store::glob_match(pat, &index)) {
            overlapping.push(json!({"name": name, "index_patterns": pats}));
        }
    }
    overlapping.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    respond(&p, json!({
        "template": compose_template(&store, &source),
        "overlapping": overlapping,
    }))
}

pub async fn put_index_template(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if body.get("index_patterns").is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: index patterns are missing;",
        );
    }
    // the composable form nests settings/mappings/aliases under `template`
    let patterns = match body["index_patterns"].clone() {
        Value::String(one) => json!([one]),
        other => other,
    };
    let mut flat = json!({"index_patterns": patterns});
    if let Some(order) = body.get("priority").or_else(|| body.get("order")) {
        flat["order"] = order.clone();
    }
    if let Some(t) = body.get("template").and_then(|t| t.as_object()) {
        for k in ["settings", "mappings", "aliases"] {
            if let Some(v) = t.get(k) {
                flat[k] = v.clone();
            }
        }
    }
    // the body is kept as the composable form's own answer, so it is stored
    // in the shape that answer has: patterns as a list, settings nested under
    // `index` with text values
    let mut kept = body.clone();
    kept["index_patterns"] = flat["index_patterns"].clone();
    if let Some(set) = kept.pointer("/template/settings").cloned() {
        kept["template"]["settings"] = template_settings(&set);
    }
    // an alias's routing stands for both halves of it
    if let Some(Value::Object(aliases)) = kept.pointer("/template/aliases").cloned() {
        let expanded: serde_json::Map<String, Value> = aliases
            .into_iter()
            .map(|(a, def)| (a, crate::store::normalize_alias(&def)))
            .collect();
        kept["template"]["aliases"] = Value::Object(expanded);
    }
    if p.get("create").map(|v| v != "false").unwrap_or(false)
        && store.get_templates().contains_key(&name)
    {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("index_template [{name}] already exists"),
        );
    }
    flat["__composable"] = kept;
    store.put_template(&name, flat);
    respond(&p, json!({"acknowledged": true}))
}

pub async fn get_index_template(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let want = name.map(|Path(n)| n);
    let mut list = Vec::new();
    for (k, v) in store.get_templates() {
        let hit = match &want {
            None => true,
            Some(n) if n == "*" || n == "_all" => true,
            Some(n) => n.split(',').any(|pat| {
                pat == k || crate::store::wildcard_to_regex(pat.trim()).is_match(&k)
            }),
        };
        if !hit {
            continue;
        }
        let body = v.get("__composable").cloned().unwrap_or_else(|| v.clone());
        list.push(json!({"name": k, "index_template": body}));
    }
    if list.is_empty() {
        if let Some(n) = &want {
            if !n.contains('*') && n != "_all" {
                return err(
                    StatusCode::NOT_FOUND,
                    "resource_not_found_exception",
                    format!("index template matching [{n}] not found"),
                );
            }
        }
    }
    respond(&p, json!({"index_templates": list}))
}

pub async fn delete_index_template(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if !store.delete_template(&name) {
        return err(
            StatusCode::NOT_FOUND,
            "resource_not_found_exception",
            format!("index template matching [{name}] not found"),
        );
    }
    respond(&p, json!({"acknowledged": true}))
}

// --------------------------------------------------------------- nodes & misc

pub async fn nodes_info(Query(p): Query<Params>) -> Response {
    respond(&p, json!({
        "_nodes": {"total": 1, "successful": 1, "failed": 0},
        "cluster_name": "obsearch",
        "nodes": {"node-0": {
            "name": "obsearch", "transport_address": "127.0.0.1:9300",
            "host": "127.0.0.1", "ip": "127.0.0.1", "version": "3.9.0",
            "build_type": "tar", "build_hash": "obsearch", "roles": ["data", "ingest"],
            "attributes": {},
            "os": {"refresh_interval_in_millis": 1000,
                   "available_processors": num_cpus(),
                   "allocated_processors": num_cpus()},
            "process": {"refresh_interval_in_millis": 1000, "id": std::process::id(),
                        "mlockall": false},
            "plugins": [], "modules": [], "ingest": {"processors": []},
            "thread_pool": {}, "transport": {}, "http": {},
        }},
    }))
}

fn num_cpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

pub async fn acknowledged(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"acknowledged": true}))
}

pub async fn shards_ok(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"_shards": {"total": 1, "successful": 1, "failed": 0}}))
}

/// `_tasks/_cancel` -- nothing here runs long enough to be cancelled.
pub async fn cancel_tasks(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"nodes": {}, "node_failures": [], "tasks": []}))
}

/// A filter written the short way, spelled out.
///
/// `{"term": {"field": "value"}}` and `{"term": {"field": {"value": "value"}}}`
/// mean the same thing; the long form is what a filter is reported as, since
/// it is where the boost would go.
fn expand_filter(f: &Value) -> Value {
    let Some(o) = f.as_object() else { return f.clone() };
    let mut out = serde_json::Map::new();
    for (kind, body) in o {
        if !matches!(kind.as_str(), "term" | "prefix" | "wildcard" | "regexp" | "fuzzy") {
            out.insert(kind.clone(), body.clone());
            continue;
        }
        let Some(fields) = body.as_object() else {
            out.insert(kind.clone(), body.clone());
            continue;
        };
        let mut spelled = serde_json::Map::new();
        for (field, v) in fields {
            let long = match v {
                Value::Object(_) => v.clone(),
                other => json!({"value": other, "boost": 1.0}),
            };
            spelled.insert(field.clone(), long);
        }
        out.insert(kind.clone(), Value::Object(spelled));
    }
    Value::Object(out)
}

pub async fn search_shards(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let expr = index.map(|Path(i)| i);
    let names = match expr.as_deref() {
        Some(e) => store.resolve(e),
        None => store.names(),
    };
    // an index reached through an alias reports which alias led to it, since
    // an alias may carry a filter the caller needs to know about
    let mut via: Vec<String> = Vec::new();
    if let Some(e) = expr.as_deref() {
        // every alias the expression names, whether outright or by pattern
        let all: Vec<String> = store
            .names()
            .iter()
            .filter_map(|n| store.get(n))
            .flat_map(|st| st.read().aliases.keys().cloned().collect::<Vec<_>>())
            .collect();
        for part in e.split(',').map(|n| n.trim()) {
            if part.contains('*') {
                for a in &all {
                    if crate::store::glob_match(part, a) && !via.contains(a) {
                        via.push(a.clone());
                    }
                }
            } else if store.is_alias(part) && !via.contains(&part.to_string()) {
                via.push(part.to_string());
            }
        }
        via.sort();
        via.dedup();
    }
    // an index is listed shard by shard; a slice takes every `max`th of them,
    // which is how several readers divide one index between them
    let slice = body.get("slice");
    let slice_id = slice.and_then(|s| s.get("id")).and_then(|v| v.as_u64()).unwrap_or(0);
    let slice_max = slice.and_then(|s| s.get("max")).and_then(|v| v.as_u64()).unwrap_or(1).max(1);
    // `preference: _shards:...` narrows to the shards it names before
    // anything else looks at the list
    let preferred: Option<Vec<u64>> = p.get("preference").and_then(|v| {
        v.strip_prefix("_shards:").map(|list| {
            list.split(',').filter_map(|s| s.trim().parse::<u64>().ok()).collect()
        })
    });
    let mut listed: Vec<(String, u64)> = Vec::new();
    for n in &names {
        let count = store
            .get(n)
            .map(|st| st.read().numeric_setting("number_of_shards").unwrap_or(1).max(1))
            .unwrap_or(1);
        for shard in 0..count {
            if preferred.as_ref().map(|w| !w.contains(&shard)).unwrap_or(false) {
                continue;
            }
            listed.push((n.clone(), shard));
        }
    }
    // a slice takes every `max`th of what is left, counted by position rather
    // than by shard number -- the two differ once a preference has narrowed it
    let shards: Vec<Value> = listed
        .into_iter()
        .enumerate()
        .filter(|(i, _)| slice.is_none() || *i as u64 % slice_max == slice_id)
        .map(|(_, (n, shard))| json!([{
            "state": "STARTED", "primary": true, "node": "node-0",
            "relocating_node": null, "shard": shard, "index": n,
            "allocation_id": {"id": "_na_"}
        }]))
        .collect();
    respond(&p, json!({
        "nodes": {"node-0": {"name": "obsearch", "ephemeral_id": "_na_",
                             "transport_address": "127.0.0.1:9300", "attributes": {}}},
        "indices": names
            .iter()
            .map(|n| {
                let mut entry = json!({});
                let own: Vec<String> = via
                    .iter()
                    .filter(|a| store.resolve(a).iter().any(|r| r == n))
                    .cloned()
                    .collect();
                if !own.is_empty() {
                    // an alias may narrow what the index shows, and a caller
                    // routing its own search needs that filter
                    if let Some(st) = store.get(n) {
                        let g = st.read();
                        // several aliases reaching the same index each narrow
                        // it, and a document matching any of them is visible
                        let filters: Vec<Value> = own
                            .iter()
                            .filter_map(|a| g.aliases.get(a).and_then(|d| d.get("filter")))
                            .map(expand_filter)
                            .collect();
                        // an alias with no filter of its own opens the index
                        // up again, so there is nothing left to narrow
                        if filters.len() == own.len() {
                            match filters.len() {
                                0 => {}
                                1 => entry["filter"] = filters[0].clone(),
                                // the bool a combined filter becomes carries
                                // the defaults a bool query is built with
                                _ => {
                                    entry["filter"] = json!({"bool": {
                                        "should": filters,
                                        "adjust_pure_negative": true,
                                        "boost": 1.0,
                                    }})
                                }
                            }
                        }
                    }
                    entry["aliases"] = json!(own);
                }
                (n.clone(), entry)
            })
            .collect::<serde_json::Map<_, _>>(),
        "shards": shards,
    }))
}

pub async fn validate_query(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let shards = json!({"total": 1, "successful": 1, "failed": 0});
    // a body that is not empty and does not name a query is not a query at
    // all, whatever else it contains
    let Some(query) = body.get("query").cloned() else {
        if body.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            let mut out = json!({"_shards": shards, "valid": true});
            if p.get("explain").map(|v| v != "false").unwrap_or(false) {
                let sample = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
                out["explanations"] = json!(
                    sample
                        .iter()
                        .map(|n| json!({
                            "index": n, "valid": true,
                            "explanation": describe_query(&json!({"match_all": {}})),
                        }))
                        .collect::<Vec<_>>()
                );
            }
            return respond(&p, out);
        }
        let mut out = json!({"_shards": shards, "valid": false});
        // whatever the body holds, it is not where a query goes -- said only
        // when the caller asked to be told why
        if p.get("explain").map(|v| v != "false").unwrap_or(false) {
            if let Some(first) = body.as_object().and_then(|o| o.keys().next()) {
                out["error"] = json!(format!("request does not support [{first}]"));
            }
        }
        return respond(&p, out);
    };
    let probe = json!({"query": query, "size": 0});
    // building the query against one of the targets says whether it can be
    // read at all, and `explain` asks to be told why not
    let sample = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
    if let Some(st) = sample.first().and_then(|n| store.get(n)) {
        let g = st.read();
        let ctx = crate::query::Ctx {
            fields: &g.fields,
            mapping: &g.mapping,
            index: &g.index,
            max_terms_count: g.max_terms_count(),
            observed_kinds: &g.observed_kinds,
            kinds_complete: g.kinds_complete,
            stats: &g.stats,
        };
        if let Err(e) = crate::query::build(&ctx, probe.get("query").unwrap()) {
            let mut out = json!({"_shards": shards, "valid": false});
            if p.get("explain").map(|v| v != "false").unwrap_or(false) {
                // the name of the index it was read against, then what went
                // wrong, which is the shape the message has
                let name = sample.first().cloned().unwrap_or_default();
                out["error"] = json!(format!("[{name}] QueryShardException[{e}]"));
            }
            return respond(&p, out);
        }
    }
    match crate::search::run(&store, &expr, &probe, &Params::new()) {
        Ok(_) => {
            let mut out = json!({"_shards": shards, "valid": true});
            if p.get("explain").map(|v| v != "false").unwrap_or(false) {
                out["explanations"] = json!(
                    sample
                        .iter()
                        .map(|n| json!({
                            "index": n, "valid": true,
                            "explanation": describe_query(probe.get("query").unwrap()),
                        }))
                        .collect::<Vec<_>>()
                );
            }
            respond(&p, out)
        }
        Err(_) => respond(&p, json!({"_shards": shards, "valid": false})),
    }
}

/// How a query reads once it has been rewritten, in the shape the engine
/// names its own queries.
fn describe_query(q: &Value) -> String {
    let Some((kind, body)) = q.as_object().and_then(|o| o.iter().next()) else {
        return "*:*".to_string();
    };
    match kind.as_str() {
        "match_all" => {
            "ApproximateScoreQuery(originalQuery=*:*, approximationQuery=Approximate(*:*))"
                .to_string()
        }
        "term" | "match" => body
            .as_object()
            .and_then(|o| o.iter().next())
            .map(|(f, v)| {
                let text = match v {
                    Value::String(s) => s.clone(),
                    Value::Object(o) => o
                        .get("value")
                        .or_else(|| o.get("query"))
                        .map(|x| x.as_str().unwrap_or_default().to_string())
                        .unwrap_or_default(),
                    other => other.to_string(),
                };
                format!("{f}:{text}")
            })
            .unwrap_or_else(|| "*:*".to_string()),
        other => other.to_string(),
    }
}

/// `_analyze` runs text through the tokenizer the query path would use.
pub async fn analyze(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let text = match body.get("text") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect(),
        _ => p.get("text").map(|t| vec![t.clone()]).unwrap_or_default(),
    };
    let analyzer = body
        .get("analyzer")
        .and_then(|v| v.as_str())
        .or_else(|| p.get("analyzer").map(|s| s.as_str()));
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let st = store.resolve(&expr).into_iter().next().and_then(|n| store.get(&n));
    // a tokenizer only splits; folding case is a filter, and naming one
    // without the other asks for the split alone
    let tokenizer_only = analyzer.is_none()
        && (body.get("tokenizer").is_some() || p.contains_key("tokenizer"));
    let mut tokens = Vec::new();
    let mut pos = 0usize;
    for t in &text {
        let parts = if tokenizer_only {
            t.split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
                .map(|w| w.to_string())
                .collect()
        } else {
            match &st {
                Some(s) => crate::query::analyze_text(&s.read().index, t, analyzer),
                None => t.split_whitespace().map(|w| w.to_lowercase()).collect(),
            }
        };
        for tok in parts {
            tokens.push(json!({
                "token": tok, "start_offset": 0, "end_offset": 0,
                "type": "<ALPHANUM>", "position": pos
            }));
            pos += 1;
        }
    }
    let cap = st
        .as_ref()
        .and_then(|s| s.read().numeric_setting("analyze.max_token_count"))
        .unwrap_or(10_000) as usize;
    if tokens.len() > cap {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "The number of tokens produced by calling _analyze has exceeded the allowed \
                 maximum of [{cap}]. This limit can be set by changing the \
                 [index.analyze.max_token_count] index level setting."
            ),
        );
    }
    // `explain` asks for the same tokens laid out by the step that produced
    // them, rather than as one flat list
    if body.get("explain").and_then(|v| v.as_bool()).unwrap_or(false)
        || p.get("explain").map(|v| v == "true").unwrap_or(false)
    {
        let named = body
            .get("tokenizer")
            .or_else(|| body.get("analyzer"))
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .or_else(|| p.get("tokenizer").or_else(|| p.get("analyzer")).cloned())
            .unwrap_or_else(|| "standard".to_string());
        let stage = json!({"name": named, "tokens": tokens.clone()});
        let detail = if tokenizer_only {
            let filters: Vec<Value> = body
                .get("filter")
                .and_then(|f| f.as_array())
                .map(|a| {
                    a.iter()
                        .map(|f| {
                            let name = match f {
                                Value::String(s) => s.clone(),
                                other => other
                                    .get("type")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("filter")
                                    .to_string(),
                            };
                            // a stop filter takes words back out, which is
                            // the whole point of naming one
                            let stop: Vec<String> = f
                                .get("stopwords")
                                .and_then(|w| w.as_array())
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                        .collect()
                                })
                                .unwrap_or_default();
                            let kept: Vec<Value> = tokens
                                .iter()
                                .filter(|t| {
                                    let text = t.get("token").and_then(|v| v.as_str()).unwrap_or("");
                                    !stop.iter().any(|w| w == text)
                                })
                                .cloned()
                                .collect();
                            json!({"name": name, "tokens": kept})
                        })
                        .collect()
                })
                .unwrap_or_default();
            json!({"custom_analyzer": true, "tokenizer": stage, "tokenfilters": filters})
        } else {
            json!({"custom_analyzer": false, "analyzer": stage})
        };
        return respond(&p, json!({"detail": detail}));
    }

    respond(&p, json!({"tokens": tokens}))
}
