//! Index State Management: what an index should do to itself as it ages.
//!
//! A policy is a set of states. An index sits in one of them, does what that
//! state says to do, and moves on when a transition's condition is met -- when
//! it is a week old, when it holds a million documents, when it has grown past
//! fifty gigabytes. It is what turns "delete logs after thirty days" from
//! something somebody has to remember into something the cluster does.
//!
//! Policies and what each index is doing under one live in an index of their
//! own, `.opendistro-ism-config`, which is where OpenSearch keeps them: they
//! outlive a restart, and a cluster that has been running for a month can be
//! asked what it has been doing.

use serde_json::{Value, json};

use crate::store::Store;

pub mod actions;
pub mod engine;

/// Where policies and the state of each managed index are kept.
pub const CONFIG_INDEX: &str = ".opendistro-ism-config";

/// How often, in milliseconds, a managed index is looked at.
///
/// OpenSearch's default is five minutes. It is a cluster setting there and
/// here, because a suite that wants to watch a policy work cannot wait five
/// minutes for each step of it.
pub fn job_interval_ms(store: &Store) -> u64 {
    store
        .cluster_setting("plugins.index_state_management.job_interval")
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .map(|minutes| minutes * 60_000)
        .or_else(|| {
            std::env::var("BOOSTSEARCH_ISM_INTERVAL_MS").ok().and_then(|v| v.parse().ok())
        })
        .unwrap_or(5 * 60_000)
}

/// Whether index management runs at all.
pub fn enabled(store: &Store) -> bool {
    store
        .cluster_setting("plugins.index_state_management.enabled")
        .and_then(|v| v.as_bool().or_else(|| v.as_str().map(|s| s != "false")))
        .unwrap_or(true)
}

/// Read one record out of the config index.
pub fn read(store: &Store, id: &str) -> Option<Value> {
    let st = store.get(CONFIG_INDEX)?;
    let g = st.read();
    crate::api::read_source(&g, id)
}

/// Write one record into the config index.
pub fn put(store: &Store, id: &str, body: Value) -> Result<(), String> {
    let st = store.ensure(CONFIG_INDEX).map_err(|e| e.to_string())?;
    let mut g = st.write();
    let raw = body.to_string();
    crate::api::write_doc_versioned(&mut g, id, body, "index", Some(raw), None)
        .map(|_| ())
        .map_err(|_| format!("[{id}] could not be written"))?;
    let _ = g.refresh();
    Ok(())
}

/// Forget one record.
pub fn remove(store: &Store, id: &str) -> bool {
    let Some(st) = store.get(CONFIG_INDEX) else { return false };
    let mut g = st.write();
    if crate::api::read_source(&g, id).is_none() {
        return false;
    }
    let (_, _) = crate::api::delete_doc(&mut g, id);
    let _ = g.refresh();
    true
}

/// Every record of one kind, as `(id, body)`.
pub fn all(store: &Store, kind: &str) -> Vec<(String, Value)> {
    let Some(st) = store.get(CONFIG_INDEX) else { return Vec::new() };
    let g = st.read();
    g.all_ids()
        .into_iter()
        .filter_map(|id| {
            let body = crate::api::read_source(&g, &id)?;
            body.get(kind).is_some().then_some((id, body))
        })
        .collect()
}

/// The id a policy is stored under.
pub fn policy_id(name: &str) -> String {
    format!("policy:{name}")
}

/// The id an index's state is stored under.
pub fn managed_id(index: &str) -> String {
    format!("managed:{index}")
}

/// What an index is doing under its policy, right now.
///
/// This is what `explain` answers with and what a tick reads and writes: the
/// policy it is under, the state it is in, when it entered that state, when
/// it was last looked at, and what went wrong if anything did.
pub fn managed(store: &Store, index: &str) -> Option<Value> {
    read(store, &managed_id(index))
}

/// Put an index under a policy.
pub fn attach(store: &Store, index: &str, policy_id_name: &str) -> Result<(), String> {
    let Some(policy) = read(store, &policy_id(policy_id_name)) else {
        return Err(format!("Policy with id {policy_id_name} does not exist"));
    };
    let default_state = policy
        .pointer("/policy/default_state")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let now = crate::store::now_millis();
    put(
        store,
        &managed_id(index),
        json!({
            "managed_index": {
                "index": index,
                "policy_id": policy_id_name,
                // the policy is copied in as it stands: changing a policy
                // does not change what an index already under it is doing,
                // which is what `change_policy` is for
                "policy": policy.get("policy").cloned().unwrap_or(json!({})),
                "policy_seq_no": policy.get("_seq_no").cloned().unwrap_or(json!(0)),
                "enabled": true,
                "enabled_time": now,
                "last_updated_time": now,
                "state": {
                    "name": default_state,
                    "start_time": now,
                },
                "action": Value::Null,
                "info": Value::Null,
                "retry_count": 0,
            }
        }),
    )
}

/// The policy an index would pick up on its own.
///
/// A policy may name the indices it applies to; an index created afterwards
/// that matches one is managed without anybody attaching it, and where two
/// match, the one that says it is more important wins.
pub fn template_for(store: &Store, index: &str) -> Option<String> {
    if index.starts_with('.') {
        return None;
    }
    let mut best: Option<(i64, String)> = None;
    for (id, body) in all(store, "policy") {
        // a policy that names no patterns claims nothing, and is passed over
        // rather than ending the search
        let templates = match body.pointer("/policy/ism_template") {
            Some(Value::Array(a)) => a.clone(),
            Some(one @ Value::Object(_)) => vec![one.clone()],
            _ => continue,
        };
        for template in templates {
            let patterns = template
                .get("index_patterns")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let matched = patterns.iter().filter_map(|p| p.as_str()).any(|p| {
                p == index || crate::store::glob_match(p, index)
            });
            if !matched {
                continue;
            }
            let priority = template.get("priority").and_then(|v| v.as_i64()).unwrap_or(0);
            let name = id.trim_start_matches("policy:").to_string();
            if best.as_ref().map(|(p, _)| priority > *p).unwrap_or(true) {
                best = Some((priority, name));
            }
        }
    }
    best.map(|(_, name)| name)
}
