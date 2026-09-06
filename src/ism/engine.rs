//! One tick of index management: look at an index, do what its state says,
//! and move it on if a transition says to.
//!
//! A tick is deliberately small. It runs the actions of the state the index
//! is in, one at a time, remembering which it has finished, and only when
//! they are all done does it look at the transitions. An action that fails is
//! retried on the next tick rather than being skipped, and an index whose
//! retries run out is left where it is with the reason written down, which is
//! what `explain` shows and what `retry` clears.

use serde_json::{Value, json};

use super::{actions, managed_id, put, read};
use crate::store::Store;

/// How many times an action is tried before the index is left alone.
const RETRIES: i64 = 3;

/// Look at every index under a policy, once.
pub fn tick(store: &Store) {
    if !super::enabled(store) {
        return;
    }
    // an index made since the last tick may have a policy waiting for it
    adopt_new_indices(store);
    for (id, body) in super::all(store, "managed_index") {
        let index = id.trim_start_matches("managed:").to_string();
        if let Some(next) = advance(store, &index, &body) {
            let _ = put(store, &managed_id(&index), next);
        }
    }
}

/// Indices that match a policy's template and are not managed yet.
fn adopt_new_indices(store: &Store) {
    for index in store.names() {
        if index.starts_with('.') || super::managed(store, &index).is_some() {
            continue;
        }
        if let Some(policy) = super::template_for(store, &index) {
            let _ = super::attach(store, &index, &policy);
        }
    }
}

/// What this index's record should say after one tick, if anything changed.
fn advance(store: &Store, index: &str, body: &Value) -> Option<Value> {
    let managed = body.get("managed_index")?;
    if managed.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
        return None;
    }
    // an index that has gone is no longer anything's business
    if store.get(index).is_none() {
        return None;
    }
    let policy = managed.get("policy")?;
    let state_name =
        managed.pointer("/state/name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let state = named_state(policy, &state_name)?;
    let now = crate::store::now_millis();
    let mut managed = managed.clone();

    // Which action is next: the one after the last that finished. An action
    // is remembered by its position, because two actions of the same kind in
    // one state are two different steps.
    let done = managed.pointer("/action/index").and_then(|v| v.as_i64()).unwrap_or(-1);
    let failed = managed.pointer("/action/failed").and_then(|v| v.as_bool()).unwrap_or(false);
    let retries = managed.get("retry_count").and_then(|v| v.as_i64()).unwrap_or(0);
    if failed && retries >= RETRIES {
        return None;
    }
    let list = state.get("actions").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let next = if failed { done } else { done + 1 };
    if let Some(action) = list.get(next.max(0) as usize) {
        let kind = action
            .as_object()
            .and_then(|o| o.keys().find(|k| *k != "timeout" && *k != "retry"))
            .cloned()
            .unwrap_or_default();
        let outcome = actions::run(store, index, &kind, action);
        let object = managed.as_object_mut()?;
        object.insert("last_updated_time".into(), json!(now));
        match outcome {
            Ok(note) => {
                object.insert(
                    "action".into(),
                    json!({"name": kind, "index": next, "start_time": now, "failed": false}),
                );
                object.insert("retry_count".into(), json!(0));
                object.insert("info".into(), json!({"message": note}));
            }
            Err(why) => {
                object.insert(
                    "action".into(),
                    json!({"name": kind, "index": next, "start_time": now, "failed": true}),
                );
                object.insert("retry_count".into(), json!(retries + 1));
                object.insert("info".into(), json!({"message": why}));
            }
        }
        return Some(json!({"managed_index": managed}));
    }

    // every action of this state is done, so where does the index go next
    let entered = managed.pointer("/state/start_time").and_then(|v| v.as_i64()).unwrap_or(now);
    let transitions =
        state.get("transitions").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for transition in transitions {
        let Some(to) = transition.get("state_name").and_then(|v| v.as_str()) else { continue };
        let conditions = transition.get("conditions").cloned().unwrap_or(json!({}));
        if !met(store, index, &conditions, entered) {
            continue;
        }
        let object = managed.as_object_mut()?;
        object.insert("state".into(), json!({"name": to, "start_time": now}));
        object.insert("action".into(), Value::Null);
        object.insert("retry_count".into(), json!(0));
        object.insert("last_updated_time".into(), json!(now));
        object.insert(
            "info".into(),
            json!({"message": format!("Transitioning to {to}")}),
        );
        return Some(json!({"managed_index": managed}));
    }
    None
}

fn named_state<'a>(policy: &'a Value, name: &str) -> Option<&'a Value> {
    policy.get("states")?.as_array()?.iter().find(|s| {
        s.get("name").and_then(|v| v.as_str()) == Some(name)
    })
}

/// Whether a transition's conditions are all met.
///
/// A transition with no conditions is taken as soon as the state's actions
/// are done, which is how a policy says "then this".
fn met(store: &Store, index: &str, conditions: &Value, entered: i64) -> bool {
    let Some(conditions) = conditions.as_object() else { return true };
    if conditions.is_empty() {
        return true;
    }
    let now = crate::store::now_millis();
    for (name, value) in conditions {
        let ok = match name.as_str() {
            "min_index_age" => {
                let age = creation_age(store, index).unwrap_or(0);
                age >= duration_ms(value).unwrap_or(i64::MAX)
            }
            // how long the index has been in this state, which is not the
            // same as how old it is
            "min_state_age" => now - entered >= duration_ms(value).unwrap_or(i64::MAX),
            "min_doc_count" => {
                let count = documents(store, index).unwrap_or(0);
                count >= value.as_i64().unwrap_or(i64::MAX)
            }
            "min_size" => {
                let size = crate::store::Store::index_size(store, index) as i64;
                size >= bytes(value).unwrap_or(i64::MAX)
            }
            // a rollover condition is about the alias the index rolled from
            "min_rollover_age" => {
                let rolled = rollover_time(store, index).unwrap_or(i64::MAX);
                now - rolled >= duration_ms(value).unwrap_or(i64::MAX)
            }
            // a cron condition is a schedule, and a tick is not the place to
            // work out whether one has come round; it is taken as not met
            "cron" => false,
            _ => true,
        };
        if !ok {
            return false;
        }
    }
    true
}

fn documents(store: &Store, index: &str) -> Option<i64> {
    let st = store.get(index)?;
    let g = st.read();
    Some(g.reader.searcher().num_docs() as i64)
}

/// How long ago the index was made.
fn creation_age(store: &Store, index: &str) -> Option<i64> {
    let st = store.get(index)?;
    let g = st.read();
    let made = g.setting("creation_date").and_then(|v| v.parse::<i64>().ok())?;
    Some(crate::store::now_millis() - made)
}

fn rollover_time(store: &Store, index: &str) -> Option<i64> {
    let st = store.get(index)?;
    let g = st.read();
    g.setting("rollover_time").and_then(|v| v.parse::<i64>().ok())
}

/// `30d`, `1h`, `10m` -- and a plain number is milliseconds.
pub fn duration_ms(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(n);
    }
    let text = value.as_str()?.trim();
    let (number, unit) = text.split_at(text.find(|c: char| c.is_alphabetic()).unwrap_or(text.len()));
    let number: i64 = number.trim().parse().ok()?;
    Some(match unit.trim() {
        "d" => number * 86_400_000,
        "h" => number * 3_600_000,
        "m" => number * 60_000,
        "s" => number * 1_000,
        "ms" | "" => number,
        _ => return None,
    })
}

/// `50gb`, `1tb`, `100mb` -- and a plain number is bytes.
pub fn bytes(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(n);
    }
    let text = value.as_str()?.trim().to_ascii_lowercase();
    let (number, unit) = text.split_at(text.find(|c: char| c.is_alphabetic()).unwrap_or(text.len()));
    let number: f64 = number.trim().parse().ok()?;
    let scale = match unit.trim() {
        "b" | "" => 1.0,
        "kb" => 1024.0,
        "mb" => 1024.0 * 1024.0,
        "gb" => 1024.0 * 1024.0 * 1024.0,
        "tb" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        "pb" => 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number * scale) as i64)
}

/// What `explain` says about one index.
pub fn explain(store: &Store, index: &str) -> Value {
    let Some(body) = read(store, &managed_id(index)) else {
        return json!({"index.plugins.index_state_management.policy_id": Value::Null});
    };
    let managed = body.get("managed_index").cloned().unwrap_or(json!({}));
    let policy_id = managed.get("policy_id").cloned().unwrap_or(Value::Null);
    json!({
        "index.plugins.index_state_management.policy_id": policy_id,
        "index.opendistro.index_state_management.policy_id": policy_id,
        "index": index,
        "index_uuid": store
            .get(index)
            .map(|st| st.read().uuid.clone())
            .unwrap_or_default(),
        "policy_id": policy_id,
        "policy_seq_no": managed.get("policy_seq_no").cloned().unwrap_or(json!(0)),
        "policy_primary_term": 1,
        "index_creation_date": store
            .get(index)
            .and_then(|st| st.read().setting("creation_date").and_then(|v| v.parse::<i64>().ok()))
            .unwrap_or(0),
        "enabled": managed.get("enabled").cloned().unwrap_or(json!(true)),
        "state": managed.get("state").cloned().unwrap_or(Value::Null),
        "action": managed.get("action").cloned().unwrap_or(Value::Null),
        "retry_info": {
            "failed": managed.pointer("/action/failed").cloned().unwrap_or(json!(false)),
            "consumed_retries": managed.get("retry_count").cloned().unwrap_or(json!(0)),
        },
        "info": managed.get("info").cloned().unwrap_or(Value::Null),
    })
}
