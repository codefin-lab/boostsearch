//! Snapshot management: taking backups on a schedule, and throwing away the
//! ones nobody needs any longer.
//!
//! A policy says when to take a snapshot and when to delete one, and the
//! cluster does both without anybody remembering to. It is the same kind of
//! job index state management's transforms and rollups are -- a document in
//! `.opendistro-ism-config` with a second document beside it saying how far it
//! has got -- so it is kept, scheduled and explained the same way.
//!
//! The two halves of a policy run independently: creation takes a snapshot
//! when its schedule says to, deletion looks at what the repository holds and
//! throws away whatever the condition says is surplus. Each half remembers
//! the state it is in and what it last did, which is what `_explain` answers.

use serde_json::{Value, json};

use super::jobs;
use crate::store::Store;

/// The schema version the plugin writes a policy with.
pub const SCHEMA_VERSION: i64 = 24;

/// How often a policy is looked at, which is what the plugin schedules one on:
/// every minute, whatever the creation and deletion schedules say. The
/// schedules themselves decide whether anything happens.
const SWEEP_MS: i64 = 60_000;

/// What a snapshot is named after the policy, where the policy does not say.
const DEFAULT_DATE_FORMAT: &str = "yyyy-MM-dd'T'HH:mm:ss";

/// The id a policy is stored under. The plugin's own suffix is kept, because
/// it is what the API answers as `_id`.
pub fn policy_id(name: &str) -> String {
    format!("{name}-sm-policy")
}

/// The id a policy's metadata is stored under.
fn metadata_id(name: &str) -> String {
    format!("{name}-sm-metadata")
}

/// The name a policy has, read back out of the id it is stored under.
pub fn name_of(id: &str) -> String {
    id.strip_suffix("-sm-policy").unwrap_or(id).to_string()
}

/// A policy the caller may see, or the reason there is none to show.
pub fn held(store: &Store, name: &str) -> Option<jobs::Held> {
    jobs::read(store, &policy_id(name)).filter(|h| h.body.get("sm_policy").is_some())
}

/// Every policy there is, by the name it was written under.
pub fn all(store: &Store) -> Vec<(String, jobs::Held)> {
    jobs::all(store, "sm_policy")
}

/// A name a policy may be written under.
///
/// The plugin refuses the characters that could not be part of a snapshot's
/// name, and reports every reason it found rather than the first.
fn name_refusal(name: &str) -> Option<String> {
    const FORBIDDEN: [char; 10] = [' ', '"', '*', '\\', '<', '|', ',', '>', '/', '?'];
    let mut reasons: Vec<String> = Vec::new();
    if name.is_empty() {
        reasons.push("Policy name must not be empty.".to_string());
    }
    if name != name.to_lowercase() {
        reasons.push("Policy name must be lowercase.".to_string());
    }
    if name.chars().any(char::is_whitespace) {
        reasons.push("Policy name must not contain whitespace.".to_string());
    }
    if name.chars().any(|c| FORBIDDEN.contains(&c)) {
        reasons.push(
            "Policy name must not contain the following characters \
             [ , \", *, \\, <, |, ,, >, /, ?]."
                .to_string(),
        );
    }
    (!reasons.is_empty()).then(|| reasons.join("\n"))
}

/// A policy as it is stored, from the body a caller wrote.
///
/// Everything the plugin fills in is filled in here: the schedule the job
/// itself runs on, the deletion schedule where only a creation one was given,
/// the smallest number of snapshots a deletion will leave, and the
/// notification conditions.
pub fn parse(
    name: &str,
    body: &Value,
    now: i64,
    before: Option<&Value>,
) -> Result<Value, super::transform::Refusal> {
    let bad = |reason: String| (400u16, "illegal_argument_exception".to_string(), reason);
    if let Some(why) = name_refusal(name) {
        return Err(bad(why));
    }
    let creation = body
        .get("creation")
        .filter(|c| c.is_object())
        .ok_or_else(|| bad("Must provide the creation configuration.".to_string()))?;
    let creation_schedule = schedule_of(creation).map_err(bad)?;
    let config = body
        .get("snapshot_config")
        .filter(|c| c.is_object())
        .ok_or_else(|| bad("snapshot_config field must not be null".to_string()))?;
    if config.get("repository").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
        return Err(bad("Must provide the repository in snapshot config.".to_string()));
    }
    let repository = config["repository"].as_str().unwrap_or("").to_string();

    let mut creation_out = json!({"schedule": creation_schedule.clone()});
    // how long a creation may take before the policy gives up on it
    if let Some(limit) = creation.get("time_limit") {
        creation_out["time_limit"] = limit.clone();
    }

    // Deletion is optional. Where it is asked for it must say what to delete;
    // where it says nothing about when, it runs on the creation schedule,
    // which is what the plugin writes back.
    let deletion = match body.get("deletion").filter(|d| d.is_object()) {
        None => None,
        Some(d) => {
            let condition = d
                .get("condition")
                .filter(|c| c.is_object())
                .ok_or_else(|| bad("condition must not be null.".to_string()))?;
            let schedule = match d.get("schedule").is_some() {
                true => schedule_of(d).map_err(bad)?,
                false => creation_schedule.clone(),
            };
            let mut out = json!({"schedule": schedule, "condition": {}});
            if let Some(age) = condition.get("max_age") {
                out["condition"]["max_age"] = age.clone();
            }
            // a deletion that empties the repository is not a retention
            // policy: the plugin keeps one snapshot back unless told how many
            out["condition"]["min_count"] = condition.get("min_count").cloned().unwrap_or(json!(1));
            if let Some(max) = condition.get("max_count") {
                out["condition"]["max_count"] = max.clone();
            }
            if let Some(limit) = d.get("time_limit") {
                out["time_limit"] = limit.clone();
            }
            Some(out)
        }
    };

    let mut config_out = json!({"repository": repository});
    for key in [
        "indices",
        "ignore_unavailable",
        "include_global_state",
        "partial",
        "date_format",
        "date_format_timezone",
        "metadata",
    ] {
        if let Some(v) = config.get(key) {
            config_out[key] = v.clone();
        }
    }

    // A policy that is running keeps the time it was enabled at: `_start`
    // is what gives it a new one. Without this, every edit of a policy reset
    // its place in the schedule.
    let was_enabled = before.and_then(|b| b.get("enabled").and_then(|v| v.as_bool()));
    let enabled = body.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
    let enabled_time = match (enabled, was_enabled, before) {
        (true, Some(true), Some(b)) => b.get("enabled_time").cloned().unwrap_or(json!(now)),
        (true, _, _) => json!(now),
        (false, _, _) => Value::Null,
    };

    let mut policy = json!({
        "name": name,
        "schema_version": SCHEMA_VERSION,
        "creation": creation_out,
        "snapshot_config": config_out,
        // the job itself is looked at every minute; the schedules inside it
        // decide what happens when it is
        "schedule": {"interval": {"start_time": now, "period": 1, "unit": "Minutes"}},
        "enabled": enabled,
        "last_updated_time": now,
        "enabled_time": enabled_time,
    });
    if let Some(description) = body.get("description") {
        policy["description"] = description.clone();
    }
    if let Some(deletion) = deletion {
        policy["deletion"] = deletion;
    }
    if let Some(notification) = body.get("notification").filter(|n| n.is_object()) {
        let asked = notification.get("conditions").cloned().unwrap_or(json!({}));
        let told = |key: &str| asked.get(key).cloned().unwrap_or(json!(false));
        policy["notification"] = json!({
            "channel": notification.get("channel").cloned().unwrap_or(json!({})),
            "conditions": {
                "creation": told("creation"),
                "deletion": told("deletion"),
                "failure": told("failure"),
                "time_limit_exceeded": told("time_limit_exceeded"),
            },
        });
    }
    // the fields are written in the order the plugin writes them, so that a
    // client reading the answer as text sees what it sees there
    Ok(ordered(policy))
}

/// A policy with its fields in the order the plugin writes them.
fn ordered(policy: Value) -> Value {
    let mut out = serde_json::Map::new();
    for key in [
        "name",
        "description",
        "schema_version",
        "creation",
        "deletion",
        "snapshot_config",
        "schedule",
        "enabled",
        "last_updated_time",
        "enabled_time",
        "notification",
    ] {
        if let Some(v) = policy.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    Value::Object(out)
}

/// The schedule a half of a policy runs on, checked the way the job scheduler
/// checks one.
fn schedule_of(half: &Value) -> Result<Value, String> {
    let raw = half.get("schedule").ok_or_else(|| "schedule field must not be null".to_string())?;
    // the cron a policy is written with is the plugin's five-field one, and
    // it says how many parts it expected when it is given another number
    if let Some(cron) = raw.get("cron")
        && let Some(expr) = cron.get("expression").and_then(|v| v.as_str())
    {
        let parts = expr.split_whitespace().count();
        if parts != 5 {
            return Err(format!("Cron expression contains {parts} parts but we expect one of [5]"));
        }
    }
    jobs::parse_schedule(raw, "snapshot management", None).map_err(|(_, reason)| reason)
}

// ------------------------------------------------------------------ metadata

/// Where a policy's two halves have got to, as `_explain` answers it.
fn metadata(store: &Store, name: &str) -> Value {
    jobs::read(store, &metadata_id(name))
        .map(|h| h.body["sm_metadata"].clone())
        .unwrap_or(Value::Null)
}

fn save_metadata(store: &Store, name: &str, meta: &Value) {
    let _ = jobs::write(store, &metadata_id(name), json!({"sm_metadata": meta}), false, None);
}

/// Forget what a policy was doing, when the policy is deleted.
pub fn forget(store: &Store, name: &str) {
    jobs::delete(store, &metadata_id(name));
    jobs::forget("sm", &policy_id(name));
}

/// What a policy is doing, and what it last did.
///
/// A policy that has not run yet says only which state each of its halves
/// begins in, which is what the plugin answers for one written a moment ago.
pub fn explain(store: &Store, name: &str, held: &jobs::Held) -> Value {
    let policy = &held.body["sm_policy"];
    let meta = metadata(store, name);
    let mut out = json!({"name": name});
    let half = |which: &str| -> Option<Value> {
        policy.get(which)?;
        let mut shown = json!({
            "current_state": meta
                .pointer(&format!("/{which}/current_state"))
                .cloned()
                .unwrap_or(json!(match which {
                    "creation" => "CREATION_START",
                    _ => "DELETION_START",
                })),
            "trigger": {
                "time": meta
                    .pointer(&format!("/{which}/trigger/time"))
                    .cloned()
                    .unwrap_or_else(|| json!(next_time(policy, which, crate::store::now_millis()))),
            },
        });
        for key in ["started", "latest_execution"] {
            if let Some(v) = meta.pointer(&format!("/{which}/{key}")) {
                shown[key] = v.clone();
            }
        }
        Some(shown)
    };
    if let Some(creation) = half("creation") {
        out["creation"] = creation;
    }
    if let Some(deletion) = half("deletion") {
        out["deletion"] = deletion;
    }
    out["policy_seq_no"] = json!(held.seq_no);
    out["policy_primary_term"] = json!(held.primary_term);
    out["enabled"] = policy.get("enabled").cloned().unwrap_or(json!(false));
    out
}

/// When a half of a policy next runs, by its own schedule.
fn next_time(policy: &Value, which: &str, now: i64) -> i64 {
    policy
        .pointer(&format!("/{which}/schedule"))
        .and_then(|s| jobs::next_run(s, now))
        .unwrap_or(now + SWEEP_MS)
}

// ------------------------------------------------------------------ running

/// Look at every policy once, and run the halves whose time has come.
pub fn tick(store: &Store) {
    let now = crate::store::now_millis();
    for (id, held) in all(store) {
        let policy = &held.body["sm_policy"];
        if policy.get("enabled").and_then(|v| v.as_bool()) != Some(true) {
            continue;
        }
        let enabled_at = policy.get("enabled_time").and_then(|v| v.as_i64()).unwrap_or(0);
        let name = name_of(&id);
        for which in ["creation", "deletion"] {
            let Some(schedule) = policy.pointer(&format!("/{which}/schedule")) else { continue };
            if !jobs::due(&format!("sm-{which}"), &id, schedule, enabled_at, now) {
                continue;
            }
            match which {
                "creation" => create(store, &name, policy, now),
                _ => delete_surplus(store, &name, policy, now),
            }
        }
    }
}

/// The name a snapshot a policy takes is given: the policy's name, the instant
/// it was taken, and enough randomness that two policies firing in the same
/// second cannot collide.
fn snapshot_name(policy: &Value, name: &str, now: i64) -> String {
    let format = policy
        .pointer("/snapshot_config/date_format")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_DATE_FORMAT);
    let zone = policy
        .pointer("/snapshot_config/date_format_timezone")
        .and_then(|v| v.as_str())
        .unwrap_or("UTC");
    let stamp = format_at(now, format, zone);
    // a snapshot's name may hold no upper case, and a date format may
    // perfectly well produce some
    format!("{name}-{}-{}", stamp.to_lowercase(), random_suffix())
}

/// The instant a message in the metadata is stamped with.
fn stamped(at: i64) -> String {
    format_at(at, "yyyy-MM-dd'T'HH:mm:ss'Z'", "UTC")
}

/// A length of time a deletion condition names, in milliseconds.
fn millis_of(text: &str) -> Option<i64> {
    crate::tasks::millis_of(text).map(|ms| ms as i64)
}

/// An instant written out by a date pattern, in a named zone.
///
/// Only the fields a snapshot's name is built from are understood -- the year,
/// month, day, hour, minute and second -- which is what the plugin's default
/// pattern and the patterns anybody writes for a backup name use. Anything in
/// single quotes is a literal, as it is in Java's own patterns.
fn format_at(at: i64, pattern: &str, zone: &str) -> String {
    let seconds = at.div_euclid(1000);
    let offset = crate::tz::offset_at(zone, seconds).unwrap_or(0);
    let local = velocore::time::OffsetDateTime::from_unix_timestamp(seconds + offset as i64)
        .unwrap_or(velocore::time::OffsetDateTime::UNIX_EPOCH);
    let mut out = String::new();
    let mut rest = pattern;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('\'') {
            // two quotes in a row are one quote; anything else runs to the
            // closing quote
            match after.split_once('\'') {
                Some((literal, tail)) => {
                    out.push_str(literal);
                    rest = tail;
                }
                None => {
                    out.push_str(after);
                    break;
                }
            }
            continue;
        }
        let field = [
            ("yyyy", format!("{:04}", local.year())),
            ("MM", format!("{:02}", local.month() as u8)),
            ("dd", format!("{:02}", local.day())),
            ("HH", format!("{:02}", local.hour())),
            ("mm", format!("{:02}", local.minute())),
            ("ss", format!("{:02}", local.second())),
        ]
        .into_iter()
        .find(|(token, _)| rest.starts_with(token));
        match field {
            Some((token, written)) => {
                out.push_str(&written);
                rest = &rest[token.len()..];
            }
            None => {
                let mut chars = rest.chars();
                if let Some(c) = chars.next() {
                    out.push(c);
                }
                rest = chars.as_str();
            }
        }
    }
    out
}

/// Take one snapshot of what the policy's configuration names, here on this
/// node, and record it in the repository the way any other snapshot is
/// recorded.
///
/// Nothing is answered to a client, so there is nobody to tell that it has
/// begun: the policy waits for it, and what it writes down afterwards is
/// whether it worked.
fn take(store: &Store, repository: &str, snapshot: &str, config: &Value) -> Result<(), String> {
    let found = store
        .repositories()
        .get(repository)
        .cloned()
        .ok_or_else(|| format!("[{repository}] missing"))?;
    let to = crate::snapshot::Source::of(&found)
        .ok_or_else(|| format!("[{repository}] cannot be written to"))?;
    let expr = config.get("indices").and_then(|v| v.as_str()).unwrap_or("*");
    let lenient = config.get("ignore_unavailable").and_then(|v| v.as_bool()).unwrap_or(false);
    let indices = store.resolve(expr);
    if indices.is_empty() && !lenient {
        return Err(format!("no index matches [{expr}]"));
    }
    let global = config.get("include_global_state").and_then(|v| v.as_bool()).unwrap_or(true);
    let streams: Vec<String> = store
        .data_streams()
        .into_keys()
        .filter(|s| {
            expr.split(',').any(|part| {
                let part = part.trim();
                part == "*" || part == "_all" || part == s || crate::store::glob_match(part, s)
            })
        })
        .collect();
    let mut record = crate::api::snapshot_record(store, snapshot, indices.clone(), streams, global);
    if let Some(metadata) = config.get("metadata") {
        record["metadata"] = metadata.clone();
    }
    crate::snapshot::write_local(store, &to, snapshot, &indices, &record)?;
    if global {
        crate::snapshot::write_global(&to, snapshot, &crate::snapshot::global_state(store))
            .map_err(|e| e.to_string())?;
    }
    store.put_snapshot(repository, snapshot, record);
    Ok(())
}

/// Eight characters of the alphabet the plugin's names end in.
///
/// Two policies whose schedules fire in the same second would otherwise reach
/// the same name, and a snapshot written under a name the repository already
/// holds writes over what was being kept there.
fn random_suffix() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    crate::cluster::NodeId::random()
        .as_str()
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .take(8)
        .map(|b| ALPHABET[b as usize % ALPHABET.len()] as char)
        .collect()
}

/// Take the snapshot the policy asks for, and write down that it was taken.
fn create(store: &Store, name: &str, policy: &Value, now: i64) {
    let config = &policy["snapshot_config"];
    let repository = config["repository"].as_str().unwrap_or_default().to_string();
    let snapshot = snapshot_name(policy, name, now);
    let mut meta = metadata(store, name);
    if meta.is_null() {
        meta = json!({});
    }
    let stamp = stamped(now);
    let outcome = take(store, &repository, &snapshot, config);
    let (status, message) = match &outcome {
        Ok(()) => (
            "SUCCESS",
            format!("[{stamp}]: Snapshot {snapshot} creation has been started and completed."),
        ),
        Err(why) => ("FAILED", format!("[{stamp}]: {why}")),
    };
    meta["creation"] = json!({
        "current_state": if outcome.is_ok() { "CREATION_FINISHED" } else { "CREATION_START" },
        "trigger": {"time": next_time(policy, "creation", now)},
        "started": if outcome.is_ok() { json!([snapshot]) } else { Value::Null },
        "latest_execution": {
            "status": status,
            "start_time": now,
            "end_time": crate::store::now_millis(),
            "info": {"message": message},
        },
    });
    if outcome.is_err() {
        meta["creation"]["started"] = Value::Null;
    }
    save_metadata(store, name, &meta);
}

/// Throw away the snapshots the deletion condition says are surplus.
///
/// The condition is read the way the plugin reads it: the oldest snapshots go
/// first, an age limit deletes anything older than it, a count limit keeps
/// only that many, and `min_count` is the floor neither of them goes below --
/// a retention policy that empties the repository is not a retention policy.
fn delete_surplus(store: &Store, name: &str, policy: &Value, now: i64) {
    let condition = &policy["deletion"]["condition"];
    let repository = policy["snapshot_config"]["repository"].as_str().unwrap_or_default();
    let prefix = format!("{name}-");
    let mut held: Vec<(String, i64)> = store
        .snapshots(repository)
        .into_iter()
        .filter(|(snapshot, _)| snapshot.starts_with(&prefix))
        .map(|(snapshot, record)| {
            let taken = record["start_time_in_millis"].as_i64().unwrap_or(0);
            (snapshot, taken)
        })
        .collect();
    held.sort_by_key(|(snapshot, taken)| (*taken, snapshot.clone()));
    let floor = condition["min_count"].as_u64().unwrap_or(1) as usize;
    let mut gone: Vec<String> = Vec::new();
    let max_age = condition.get("max_age").and_then(|v| v.as_str()).and_then(millis_of);
    let max_count = condition.get("max_count").and_then(|v| v.as_u64()).map(|n| n as usize);
    for (at, (snapshot, taken)) in held.iter().enumerate() {
        if held.len() - gone.len() <= floor {
            break;
        }
        let too_old = max_age.map(|age| now - taken > age).unwrap_or(false);
        let too_many = max_count.map(|max| held.len() - at > max).unwrap_or(false);
        if too_old || too_many {
            gone.push(snapshot.clone());
        }
    }
    let stamp = stamped(now);
    let to = store.repositories().get(repository).and_then(crate::snapshot::Source::of);
    for snapshot in &gone {
        // the files go with the record: a record deleted on its own leaves a
        // snapshot in the repository that the next registration reads back
        if let Some(to) = &to {
            crate::snapshot::remove(to, snapshot);
        }
        store.remove_snapshots(repository, snapshot);
    }
    let mut meta = metadata(store, name);
    if meta.is_null() {
        meta = json!({});
    }
    meta["deletion"] = json!({
        "current_state": "DELETION_FINISHED",
        "trigger": {"time": next_time(policy, "deletion", now)},
        "started": if gone.is_empty() { Value::Null } else { json!(gone) },
        "latest_execution": {
            "status": "SUCCESS",
            "start_time": now,
            "end_time": crate::store::now_millis(),
            "info": {"message": format!("[{stamp}]: Snapshot deletion has finished.")},
        },
    });
    save_metadata(store, name, &meta);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09-17T20:19:22Z, which is 03:19 the next morning in Bangkok.
    const WHEN: i64 = 1_789_676_362_000;

    #[test]
    fn a_snapshot_name_carries_the_instant_it_was_taken() {
        let policy = json!({"snapshot_config": {"repository": "backups"}});
        let name = snapshot_name(&policy, "nightly", WHEN);
        assert!(name.starts_with("nightly-2026-09-17t20:19:22-"), "{name}");
        assert_eq!(name.len(), "nightly-2026-09-17t20:19:22-".len() + 8);
        assert_eq!(name, name.to_lowercase());
    }

    #[test]
    fn a_date_format_is_read_and_a_zone_shifts_it() {
        let policy = json!({"snapshot_config": {
            "repository": "backups",
            "date_format": "yyyy-MM-dd-HH:mm",
            "date_format_timezone": "Asia/Bangkok",
        }});
        let name = snapshot_name(&policy, "nightly", WHEN);
        assert!(name.starts_with("nightly-2026-09-18-03:19-"), "{name}");
    }

    #[test]
    fn a_quoted_part_of_a_pattern_is_a_literal() {
        assert_eq!(format_at(0, "yyyy-MM-dd'T'HH:mm:ss'Z'", "UTC"), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn a_name_is_refused_for_every_reason_it_breaks() {
        assert_eq!(name_refusal("nightly"), None);
        assert_eq!(name_refusal("Nightly"), Some("Policy name must be lowercase.".to_string()));
        let both = name_refusal("a b*").unwrap();
        assert!(both.starts_with("Policy name must not contain whitespace.\n"), "{both}");
        assert!(both.contains("must not contain the following characters"), "{both}");
    }

    #[test]
    fn a_deletion_runs_on_the_creation_schedule_where_it_names_none() {
        let asked = json!({
            "creation": {"schedule": {"cron": {"expression": "0 2 * * *", "timezone": "UTC"}}},
            "deletion": {"condition": {"max_count": 10}},
            "snapshot_config": {"repository": "backups"},
        });
        let policy = parse("nightly", &asked, 0, None).unwrap();
        assert_eq!(policy["deletion"]["schedule"], policy["creation"]["schedule"]);
        // the floor a deletion will not go below, which the caller did not name
        assert_eq!(policy["deletion"]["condition"]["min_count"], json!(1));
    }

    #[test]
    fn a_deletion_that_says_nothing_to_delete_is_refused() {
        let asked = json!({
            "creation": {"schedule": {"cron": {"expression": "0 2 * * *", "timezone": "UTC"}}},
            "deletion": {"schedule": {"cron": {"expression": "0 1 * * *", "timezone": "UTC"}}},
            "snapshot_config": {"repository": "backups"},
        });
        let (status, _, reason) = parse("nightly", &asked, 0, None).unwrap_err();
        assert_eq!(status, 400);
        assert_eq!(reason, "condition must not be null.");
    }

    #[test]
    fn a_cron_is_held_to_five_fields() {
        let asked = json!({
            "creation": {"schedule": {"cron": {"expression": "0 2 *", "timezone": "UTC"}}},
            "snapshot_config": {"repository": "backups"},
        });
        let (_, _, reason) = parse("nightly", &asked, 0, None).unwrap_err();
        assert_eq!(reason, "Cron expression contains 3 parts but we expect one of [5]");
    }
}
