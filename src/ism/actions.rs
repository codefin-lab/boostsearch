//! What a state does to an index when the index is in it.
//!
//! Each of these is the same thing a person would do through the ordinary
//! API, done for them on a schedule. They go through the same code that
//! endpoint goes through, so an index rolled over by a policy is rolled over
//! the way an index rolled over by hand is.

use serde_json::{Value, json};

use crate::store::Store;

/// Do one action, and say what happened.
///
/// An error is a reason, not a panic: the engine writes it down, counts a
/// retry, and tries again on the next tick.
pub fn run(store: &Store, index: &str, kind: &str, spec: &Value) -> Result<String, String> {
    let body = spec.get(kind).cloned().unwrap_or(json!({}));
    match kind {
        "delete" => {
            store.delete(index);
            Ok(format!("Successfully deleted index [{index}]"))
        }
        "read_only" => setting(store, index, "blocks.write", json!(true), "read only"),
        "read_write" => setting(store, index, "blocks.write", json!(false), "read write"),
        "replica_count" => {
            let count = body.get("number_of_replicas").cloned().unwrap_or(json!(1));
            setting(store, index, "number_of_replicas", count, "replica count")
        }
        "index_priority" => {
            let priority = body.get("priority").cloned().unwrap_or(json!(1));
            setting(store, index, "priority", priority, "index priority")
        }
        "close" => {
            let Some(st) = store.get(index) else { return Err(missing(index)) };
            let mut g = st.write();
            g.closed = true;
            g.save_meta();
            Ok(format!("Successfully closed index [{index}]"))
        }
        "open" => {
            let Some(st) = store.get(index) else { return Err(missing(index)) };
            let mut g = st.write();
            g.closed = false;
            g.save_meta();
            Ok(format!("Successfully opened index [{index}]"))
        }
        "force_merge" => {
            let Some(st) = store.get(index) else { return Err(missing(index)) };
            let segments =
                body.get("max_num_segments").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
            let mut g = st.write();
            g.refresh().map_err(|e| e.to_string())?;
            // merge down to the count asked for, a batch at a time, the way
            // the `_forcemerge` endpoint does it
            loop {
                let ids: Vec<boostcore::index::SegmentId> = g
                    .index
                    .searchable_segment_metas()
                    .unwrap_or_default()
                    .iter()
                    .map(|m| m.id())
                    .collect();
                if ids.len() <= segments {
                    break;
                }
                let take = ids.len() - segments + 1;
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
            Ok(format!("Successfully merged index [{index}] into {segments} segments"))
        }
        "rollover" => rollover(store, index, &body),
        "snapshot" => {
            let repository = body
                .get("repository")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "[repository] is required for a snapshot".to_string())?;
            let name = body
                .get("snapshot")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("{index}-{}", crate::store::now_millis()));
            snapshot(store, repository, &name, index)
        }
        "alias" => alias(store, index, &body),
        "allocation" | "notification" | "shrink" => {
            // an allocation is about where shards sit, a notification about
            // telling somebody, and a shrink about moving into fewer shards:
            // this engine has one node and one shard, so each of them is
            // already true
            Ok(format!("[{kind}] has nothing to do on a single node"))
        }
        other => Err(format!("Unsupported action [{other}]")),
    }
}

fn missing(index: &str) -> String {
    format!("no such index [{index}]")
}

fn setting(
    store: &Store,
    index: &str,
    key: &str,
    value: Value,
    what: &str,
) -> Result<String, String> {
    let Some(st) = store.get(index) else { return Err(missing(index)) };
    let mut g = st.write();
    let settings = g.settings.as_object_mut().ok_or("settings are not an object")?;
    settings.insert(format!("index.{key}"), value);
    g.save_meta();
    Ok(format!("Successfully set {what} on [{index}]"))
}

/// Roll the alias this index is the write index of onto a new index.
///
/// A rollover under a policy is the same rollover the endpoint does, with the
/// same conditions: if none is met, nothing happens and the action is not
/// finished, so the next tick asks again.
fn rollover(store: &Store, index: &str, body: &Value) -> Result<String, String> {
    let alias = write_alias_of(store, index)
        .ok_or_else(|| format!("index [{index}] is not the write index for any alias"))?;
    let conditions = json!({
        "max_age": body.get("min_index_age").cloned().unwrap_or(Value::Null),
        "max_docs": body.get("min_doc_count").cloned().unwrap_or(Value::Null),
        "max_size": body.get("min_size").cloned().unwrap_or(Value::Null),
    });
    let met = rollover_conditions_met(store, index, &conditions);
    if !met {
        return Err(format!("Attempting to roll over index [{index}]"));
    }
    let next = crate::api::next_rollover_name(index)
        .ok_or_else(|| format!("index name [{index}] does not end in a number to carry on from"))?;
    crate::api::roll_alias(store, &alias, index, &next, &json!({}))?;
    Ok(format!("Successfully rolled over index [{index}] to [{next}]"))
}

/// Whether any of the conditions a rollover was given is met. No condition at
/// all means roll now, which is what the endpoint does too.
fn rollover_conditions_met(store: &Store, index: &str, conditions: &Value) -> bool {
    let named: Vec<(&str, &Value)> = conditions
        .as_object()
        .map(|o| o.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.as_str(), v)).collect())
        .unwrap_or_default();
    if named.is_empty() {
        return true;
    }
    let now = crate::store::now_millis();
    named.into_iter().any(|(name, value)| match name {
        "max_age" => {
            let made = store
                .get(index)
                .and_then(|st| {
                    st.read().setting("creation_date").and_then(|v| v.parse::<i64>().ok())
                })
                .unwrap_or(now);
            now - made >= super::engine::duration_ms(value).unwrap_or(i64::MAX)
        }
        "max_docs" => {
            let count = store
                .get(index)
                .map(|st| st.read().reader.searcher().num_docs() as i64)
                .unwrap_or(0);
            count >= value.as_i64().unwrap_or(i64::MAX)
        }
        "max_size" => {
            store.index_size(index) as i64 >= super::engine::bytes(value).unwrap_or(i64::MAX)
        }
        _ => false,
    })
}

/// The alias this index is the write index of.
fn write_alias_of(store: &Store, index: &str) -> Option<String> {
    let st = store.get(index)?;
    let g = st.read();
    g.aliases
        .iter()
        .find(|(_, spec)| spec.get("is_write_index").and_then(|v| v.as_bool()).unwrap_or(false))
        .map(|(name, _)| name.clone())
        // an index with one alias and nothing said about writing is the one
        // that is written to
        .or_else(|| {
            let names: Vec<&String> = g.aliases.keys().collect();
            (names.len() == 1).then(|| names[0].clone())
        })
}

fn snapshot(store: &Store, repository: &str, name: &str, index: &str) -> Result<String, String> {
    let found = store
        .repositories()
        .get(repository)
        .cloned()
        .ok_or_else(|| format!("[{repository}] missing"))?;
    let to = crate::snapshot::Source::of(&found)
        .ok_or_else(|| format!("[{repository}] cannot be written to"))?;
    let record = json!({
        "snapshot": name,
        "indices": [index],
        "state": "SUCCESS",
        "start_time_in_millis": crate::store::now_millis(),
    });
    crate::snapshot::write(store, &to, name, &[index.to_string()], &record)
        .map_err(|e| e.to_string())?;
    store.put_snapshot(repository, name, record);
    Ok(format!("Successfully snapshotted [{index}] into [{repository}:{name}]"))
}

fn alias(store: &Store, index: &str, body: &Value) -> Result<String, String> {
    let Some(st) = store.get(index) else { return Err(missing(index)) };
    let mut g = st.write();
    let actions = body.get("actions").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for action in actions {
        let Some((kind, spec)) = action.as_object().and_then(|o| o.iter().next()) else {
            continue;
        };
        let names: Vec<String> = match spec.get("aliases").or_else(|| spec.get("alias")) {
            Some(Value::Array(a)) => {
                a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()
            }
            Some(Value::String(one)) => vec![one.clone()],
            _ => Vec::new(),
        };
        for name in names {
            match kind.as_str() {
                "add" => {
                    g.aliases.insert(name, json!({}));
                }
                "remove" => {
                    g.aliases.remove(&name);
                }
                _ => {}
            }
        }
    }
    g.save_meta();
    Ok(format!("Successfully updated the aliases of [{index}]"))
}
