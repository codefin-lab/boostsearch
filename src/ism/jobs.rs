//! What transforms and rollups have in common: where their definitions are
//! kept, when they run, whose permissions they run with, and the ids the
//! documents they write are given.
//!
//! Both are jobs in the index-management plugin's sense. A job is a document
//! in `.opendistro-ism-config` under its own id, with a second document beside
//! it -- the metadata -- saying how far it has got. The scheduler looks at
//! every job once a second and runs the ones whose time has come, one after
//! another, on the cluster manager only.

use std::collections::HashMap;
use std::sync::OnceLock;

use axum::http::StatusCode;
use axum::response::Response;
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::CONFIG_INDEX;
use crate::api::err;
use crate::store::Store;

/// The schema version both kinds of job are written with, which is the one
/// the plugin's config index mapping carries.
pub const SCHEMA_VERSION: i64 = 30;

/// A job document as it stands in the config index.
#[derive(Clone, Debug)]
pub struct Held {
    pub body: Value,
    pub seq_no: u64,
    pub primary_term: u64,
    pub version: u64,
}

// ------------------------------------------------------------------ ids

/// MurmurHash3, the 128-bit variant for 64-bit machines.
///
/// The plugin names every document a transform or a rollup writes by hashing
/// the job's id and the bucket's key with this, so that running the job again
/// overwrites what it wrote the last time rather than adding to it. The ids
/// have to be the same ones for a re-run here to overwrite a document the
/// plugin wrote, and for a document to be found by the id a client computed.
fn murmur3_x64_128(data: &[u8], seed: u64) -> (u64, u64) {
    const C1: u64 = 0x87c3_7b91_1142_53d5;
    const C2: u64 = 0x4cf5_ad43_2745_937f;
    fn fmix(mut k: u64) -> u64 {
        k ^= k >> 33;
        k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
        k ^= k >> 33;
        k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        k ^= k >> 33;
        k
    }
    let (mut h1, mut h2) = (seed, seed);
    let blocks = data.len() / 16;
    for i in 0..blocks {
        let at = i * 16;
        let mut k1 = u64::from_le_bytes(data[at..at + 8].try_into().unwrap_or([0; 8]));
        let mut k2 = u64::from_le_bytes(data[at + 8..at + 16].try_into().unwrap_or([0; 8]));
        k1 = k1.wrapping_mul(C1).rotate_left(31).wrapping_mul(C2);
        h1 ^= k1;
        h1 = h1.rotate_left(27).wrapping_add(h2).wrapping_mul(5).wrapping_add(0x52dc_e729);
        k2 = k2.wrapping_mul(C2).rotate_left(33).wrapping_mul(C1);
        h2 ^= k2;
        h2 = h2.rotate_left(31).wrapping_add(h1).wrapping_mul(5).wrapping_add(0x3849_5ab5);
    }
    let tail = &data[blocks * 16..];
    let (mut k1, mut k2) = (0u64, 0u64);
    for (i, b) in tail.iter().enumerate().skip(8) {
        k2 ^= (*b as u64) << ((i - 8) * 8);
    }
    if tail.len() > 8 {
        h2 ^= k2.wrapping_mul(C2).rotate_left(33).wrapping_mul(C1);
    }
    for (i, b) in tail.iter().enumerate().take(8) {
        k1 ^= (*b as u64) << (i * 8);
    }
    if !tail.is_empty() {
        h1 ^= k1.wrapping_mul(C1).rotate_left(31).wrapping_mul(C2);
    }
    let len = data.len() as u64;
    h1 ^= len;
    h2 ^= len;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    h1 = fmix(h1);
    h2 = fmix(h2);
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    (h1, h2)
}

/// The plugin's fixed-size id for a text: the hash above with its seed,
/// written big-endian and base64-url encoded without padding.
pub fn hash_id(text: &str) -> String {
    use base64::Engine as _;
    let (h1, h2) = murmur3_x64_128(text.as_bytes(), 72390);
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&h1.to_be_bytes());
    bytes[8..].copy_from_slice(&h2.to_be_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// How a bucket key's value is spelled inside the text an id is hashed from:
/// the way the JVM prints the value the composite handed back. A missing
/// value has a spelling of its own, so that it cannot collide with a term.
pub fn key_text(v: &Value) -> String {
    match v {
        Value::Null => "#ODFE-MAGIC-NULL-MAGIC-ODFE#".to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) if n.is_f64() => java_double(n.as_f64().unwrap_or(0.0)),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// A double the way `Double.toString` writes it, for the values a histogram
/// hands back: `5.0` rather than `5`, and exponent form outside the range
/// Java prints plainly.
pub fn java_double(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let abs = f.abs();
    if abs == 0.0 || (1e-3..1e7).contains(&abs) {
        let s = format!("{f}");
        if s.contains('.') { s } else { format!("{s}.0") }
    } else {
        // Java writes one digit before the point and an `E` exponent
        let s = format!("{f:e}");
        let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let mantissa =
            if mantissa.contains('.') { mantissa.to_string() } else { format!("{mantissa}.0") };
        format!("{mantissa}E{exp}")
    }
}

// ----------------------------------------------------------- the config index

/// The parts of a job document that are free-form: a query, named
/// aggregations, a bucket key, stats. They are declared as objects that are not
/// indexed, the way the plugin's own mapping of the config index declares them:
/// they are read back whole, never searched by what is inside them.
fn unmapped_parts() -> Value {
    let off = json!({"type": "object", "enabled": false});
    json!({"properties": {
        "transform": {"properties": {
            "data_selection_query": off, "aggregations": off, "groups": off, "schedule": off,
            "user": off,
        }},
        "transform_metadata": {"properties": {
            "after_key": off, "stats": off, "continuous_stats": off,
            "shard_id_to_global_checkpoint": off,
        }},
        "rollup": {"properties": {
            "dimensions": off, "metrics": off, "schedule": off, "user": off,
        }},
        "rollup_metadata": {"properties": {
            "after_key": off, "stats": off, "continuous": off,
        }},
    }})
}

fn config_index(store: &Store) -> Result<std::sync::Arc<crate::store::IdxLock>, String> {
    let st = store.ensure(CONFIG_INDEX).map_err(|e| e.to_string())?;
    {
        let mut g = st.write();
        // the mapping is merged once per process; merging it again is
        // harmless but not free
        static MERGED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
        let seen = MERGED.get_or_init(|| Mutex::new(Default::default()));
        if seen.lock().insert(g.uuid.clone()) {
            g.mapping.merge(&unmapped_parts());
        }
    }
    Ok(st)
}

/// Read one document of the config index with where it stands.
pub fn read(store: &Store, id: &str) -> Option<Held> {
    let st = store.get(CONFIG_INDEX)?;
    let g = st.read();
    if !g.is_live(id) {
        return None;
    }
    let body = crate::api::read_source(&g, id)?;
    Some(Held {
        body,
        seq_no: crate::api::read_seq(&g, id).unwrap_or(0),
        primary_term: g.term_of(id),
        version: g.version_of(id),
    })
}

/// Write one document of the config index: created only if it is not there
/// when `create` is set, and only over the sequence number a caller holds
/// when one is given.
pub fn write(
    store: &Store,
    id: &str,
    body: Value,
    create: bool,
    if_seq: Option<(u64, u64)>,
) -> Result<Held, Response> {
    let st = config_index(store)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "illegal_state_exception", e))?;
    let mut g = st.write();
    if let Some((seq, term)) = if_seq {
        let live = g.is_live(id);
        let held_seq = if live { crate::api::read_seq(&g, id) } else { None };
        let held_term = g.term_of(id);
        if held_seq != Some(seq) || held_term != term {
            let reason = match held_seq {
                Some(now) => format!(
                    "[{id}]: version conflict, required seqNo [{seq}], primary term [{term}]. \
                     current document has seqNo [{now}] and primary term [{held_term}]"
                ),
                None => format!(
                    "[{id}]: version conflict, required seqNo [{seq}], primary term [{term}] but \
                     no document was found"
                ),
            };
            return Err(crate::api::shared::doc_err(
                StatusCode::CONFLICT,
                "version_conflict_engine_exception",
                reason,
                &g.name,
                &g.uuid,
                g.shard_of_doc(id),
            ));
        }
    }
    let raw = body.to_string();
    let op = if create { "create" } else { "index" };
    let (answer, _) =
        crate::api::write_doc_internal(&mut g, id, body.clone(), op, Some(raw), None)?;
    let _ = g.refresh();
    Ok(Held {
        body,
        seq_no: answer.get("_seq_no").and_then(|v| v.as_u64()).unwrap_or(0),
        primary_term: answer.get("_primary_term").and_then(|v| v.as_u64()).unwrap_or(1),
        version: answer.get("_version").and_then(|v| v.as_u64()).unwrap_or(1),
    })
}

/// Delete one document, answering with its version and sequence number after
/// the delete, or `None` where there was nothing to delete.
pub fn delete(store: &Store, id: &str) -> Option<Value> {
    let st = store.get(CONFIG_INDEX)?;
    let mut g = st.write();
    if !g.is_live(id) {
        return None;
    }
    let (answer, _) = crate::api::delete_doc(&mut g, id);
    let _ = g.refresh();
    Some(answer)
}

/// Every job of one kind -- `transform` or `rollup` -- by id.
pub fn all(store: &Store, kind: &str) -> Vec<(String, Held)> {
    let Some(st) = store.get(CONFIG_INDEX) else { return Vec::new() };
    let g = st.read();
    let mut out: Vec<(String, Held)> = g
        .all_ids()
        .into_iter()
        .filter_map(|id| {
            let body = crate::api::read_source(&g, &id)?;
            body.get(kind)?;
            let held = Held {
                seq_no: crate::api::read_seq(&g, &id).unwrap_or(0),
                primary_term: g.term_of(&id),
                version: g.version_of(&id),
                body,
            };
            Some((id, held))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The metadata document of a job: the one its `metadata_id` names, and only
/// that one. A job written again under the id of one deleted before it starts
/// from nothing, as the plugin's does; finding the old job's metadata by its id
/// had the new one skip its run as already finished.
pub fn metadata_of(store: &Store, kind: &str, job_id: &str, named: Option<&str>) -> Option<Held> {
    let (doc_kind, id_field) = match kind {
        "transform" => ("transform_metadata", "transform_id"),
        _ => ("rollup_metadata", "rollup_id"),
    };
    if let Some(id) = named
        && let Some(held) = read(store, id)
        && held.body.pointer(&format!("/{doc_kind}/{id_field}")).and_then(|v| v.as_str())
            == Some(job_id)
    {
        return Some(held);
    }
    None
}

/// Delete the metadata a job left, with the job.
pub fn forget_metadata(store: &Store, kind: &str, job_id: &str) {
    let (doc_kind, id_field) = match kind {
        "transform" => ("transform_metadata", "transform_id"),
        _ => ("rollup_metadata", "rollup_id"),
    };
    for (id, held) in all(store, doc_kind) {
        if held.body[doc_kind][id_field].as_str() == Some(job_id) {
            delete(store, &id);
        }
    }
}

/// The id a new metadata document is written under.
pub fn new_metadata_id(job_id: &str) -> String {
    // twenty characters, the length of the ids the plugin's metadata is
    // written under
    let mut id = hash_id(&format!("RollupMetadata#{job_id}#{}", crate::store::now_millis()));
    id.truncate(20);
    id
}

// ------------------------------------------------------------------ schedules

/// The units an interval may be counted in, spelled the way the JVM names
/// them, with how long each is.
const UNITS: [(&str, i64); 7] = [
    ("Nanos", 0),
    ("Micros", 0),
    ("Millis", 1),
    ("Seconds", 1_000),
    ("Minutes", 60_000),
    ("Hours", 3_600_000),
    ("Days", 86_400_000),
];

/// A schedule as it is written back: `interval` with its start, period and
/// unit, or `cron` with its expression and zone. `delay` is a rollup's, which
/// the plugin writes into the schedule as `schedule_delay`.
pub fn parse_schedule(
    raw: &Value,
    what: &str,
    delay: Option<i64>,
) -> Result<Value, (String, String)> {
    let bad = |reason: String| ("illegal_argument_exception".to_string(), reason);
    let Some(obj) = raw.as_object() else {
        return Err(bad(format!("{what} schedule is null")));
    };
    if let Some(interval) = obj.get("interval") {
        let period = interval.get("period").and_then(|v| v.as_i64()).unwrap_or(0);
        let unit_raw = interval.get("unit").and_then(|v| v.as_str()).unwrap_or("");
        let Some((unit, _)) = UNITS.iter().find(|(u, _)| u.eq_ignore_ascii_case(unit_raw)) else {
            return Err(bad(format!(
                "No enum constant java.time.temporal.ChronoUnit.{}",
                unit_raw.to_ascii_uppercase()
            )));
        };
        let start = interval
            .get("start_time")
            .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or_else(crate::store::now_millis);
        let mut out = json!({"start_time": start, "period": period, "unit": unit});
        if let Some(d) = delay {
            out["schedule_delay"] = json!(d);
        }
        return Ok(json!({"interval": out}));
    }
    if let Some(cron) = obj.get("cron") {
        let expression = cron.get("expression").and_then(|v| v.as_str()).unwrap_or("");
        let zone = cron.get("timezone").and_then(|v| v.as_str()).unwrap_or("");
        if Cron::parse(expression).is_none() {
            return Err(bad(format!("Invalid cron expression: {expression}")));
        }
        let mut out = json!({"expression": expression, "timezone": zone});
        if let Some(d) = delay {
            out["schedule_delay"] = json!(d);
        }
        return Ok(json!({"cron": out}));
    }
    Err(bad(format!("{what} schedule is null")))
}

/// How long one period of an interval schedule is, where it is one.
pub fn interval_millis(schedule: &Value) -> Option<i64> {
    let interval = schedule.get("interval")?;
    let period = interval.get("period").and_then(|v| v.as_i64())?;
    let unit = interval.get("unit").and_then(|v| v.as_str())?;
    let per = UNITS.iter().find(|(u, _)| *u == unit).map(|(_, ms)| *ms)?;
    Some(period * per)
}

/// The first time after `now` a schedule says to run.
///
/// An interval runs on the grid its start time lays down: every period after
/// the start, whatever time the job was enabled at. That is what the plugin's
/// job scheduler does, and why a job enabled at 12:00:05 on a one-minute
/// schedule started at a whole minute first runs at 12:01:00.
pub fn next_run(schedule: &Value, now: i64) -> Option<i64> {
    if let Some(interval) = schedule.get("interval") {
        let every = interval_millis(schedule).filter(|ms| *ms > 0)?;
        let start = interval.get("start_time").and_then(|v| v.as_i64()).unwrap_or(0);
        let delay = interval.get("schedule_delay").and_then(|v| v.as_i64()).unwrap_or(0);
        let since = now - start;
        let next = if since < 0 { start } else { start + (since / every + 1) * every };
        return Some(next + delay);
    }
    let cron = schedule.get("cron")?;
    let expr = Cron::parse(cron.get("expression").and_then(|v| v.as_str()).unwrap_or(""))?;
    let zone = cron.get("timezone").and_then(|v| v.as_str()).unwrap_or("UTC");
    let delay = cron.get("schedule_delay").and_then(|v| v.as_i64()).unwrap_or(0);
    expr.next_after(now, zone).map(|t| t + delay)
}

/// A five-field cron expression: minute, hour, day of month, month, day of
/// week, each a list of values, ranges and steps.
struct Cron {
    fields: [Vec<bool>; 5],
    any_dom: bool,
    any_dow: bool,
}

impl Cron {
    fn parse(expr: &str) -> Option<Cron> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            return None;
        }
        let bounds = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];
        let mut fields: [Vec<bool>; 5] = Default::default();
        for (i, part) in parts.iter().enumerate() {
            let (lo, hi) = bounds[i];
            let mut set = vec![false; hi + 1];
            for item in part.split(',') {
                let (range, step) = match item.split_once('/') {
                    Some((r, s)) => (r, s.parse::<usize>().ok().filter(|s| *s > 0)?),
                    None => (item, 1),
                };
                let (from, to) = if range == "*" {
                    (lo, hi)
                } else if let Some((a, b)) = range.split_once('-') {
                    (a.parse().ok()?, b.parse().ok()?)
                } else {
                    let v: usize = range.parse().ok()?;
                    (v, if step > 1 { hi } else { v })
                };
                if from < lo || to > hi || from > to {
                    return None;
                }
                for v in (from..=to).step_by(step) {
                    set[v] = true;
                }
            }
            fields[i] = set;
        }
        // Sunday may be written as 0 or as 7
        if fields[4][7] {
            fields[4][0] = true;
        }
        Some(Cron { any_dom: parts[2] == "*", any_dow: parts[4] == "*", fields })
    }

    fn next_after(&self, now: i64, zone: &str) -> Option<i64> {
        let offset_ms = |at: i64| crate::tz::offset_at(zone, at / 1000).unwrap_or(0) as i64 * 1000;
        let mut t = (now / 60_000 + 1) * 60_000;
        // a year of minutes is the furthest any expression can be from now
        for _ in 0..(366 * 24 * 60) {
            let local = t + offset_ms(t);
            let days = local.div_euclid(86_400_000);
            let minute_of_day = local.rem_euclid(86_400_000) / 60_000;
            let (y, m, d) = civil_from_days(days);
            let _ = y;
            let dow = ((days + 4).rem_euclid(7)) as usize;
            let dom_ok = self.fields[2][d as usize];
            let dow_ok = self.fields[4][dow];
            let day_ok = match (self.any_dom, self.any_dow) {
                (true, true) => true,
                (true, false) => dow_ok,
                (false, true) => dom_ok,
                (false, false) => dom_ok || dow_ok,
            };
            if self.fields[3][m as usize]
                && day_ok
                && self.fields[1][(minute_of_day / 60) as usize]
                && self.fields[0][(minute_of_day % 60) as usize]
            {
                return Some(t);
            }
            t += 60_000;
        }
        None
    }
}

/// Days since the epoch as a year, a month and a day.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ------------------------------------------------------------------- the user

/// The user a job is written by, the way the plugin stores one, when security
/// is on and a user is asking. A job written with security off has none, and
/// runs with nobody's restrictions.
pub fn current_user(store: &Store) -> Option<Value> {
    if !store.security.enabled {
        return None;
    }
    let caller = crate::security::layer::current_caller()?;
    if caller.unrestricted {
        return None;
    }
    Some(json!({
        "name": caller.name,
        "backend_roles": caller.backend_roles,
        "roles": caller.roles,
        "custom_attribute_names": caller.attributes.keys().collect::<Vec<_>>(),
        "user_requested_tenant": caller.requested_tenant,
    }))
}

/// The caller a stored user stands for, to run a job as.
pub fn caller_of(user: &Value) -> crate::security::Caller {
    let strings = |key: &str| -> Vec<String> {
        user.get(key)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    crate::security::Caller {
        name: user.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        backend_roles: strings("backend_roles"),
        roles: strings("roles"),
        ..Default::default()
    }
}

/// Whether filtering jobs by their writer's backend roles is on.
pub fn filter_by_backend_roles(store: &Store) -> bool {
    store
        .cluster_setting("plugins.index_management.filter_by_backend_roles")
        .and_then(|v| v.as_bool().or_else(|| v.as_str().map(|s| s == "true")))
        .unwrap_or(false)
}

/// Whether the caller in hand may see and change a job written by `owner`.
///
/// With the filter off, or for an administrator, or for a job nobody's user
/// wrote, everyone may. Otherwise a caller shares at least one backend role
/// with whoever wrote it.
pub fn may_touch(store: &Store, owner: Option<&Value>) -> bool {
    if !filter_by_backend_roles(store) || !store.security.enabled {
        return true;
    }
    let Some(owner) = owner.filter(|o| !o.is_null()) else { return true };
    let Some(caller) = crate::security::layer::current_caller() else { return true };
    if caller.unrestricted || caller.roles.iter().any(|r| r == "all_access") {
        return true;
    }
    let theirs = caller_of(owner).backend_roles;
    theirs.iter().any(|r| caller.backend_roles.contains(r))
}

/// The refusal for a caller the filter above turns away, where the plugin
/// says so rather than pretending the job is not there.
pub fn user_configuration_refusal(store: &Store) -> Option<Response> {
    if !filter_by_backend_roles(store) {
        return None;
    }
    if !store.security.enabled {
        return Some(err(
            StatusCode::FORBIDDEN,
            "status_exception",
            "Filter by user backend roles in IndexManagement is not supported with security disabled",
        ));
    }
    let caller = crate::security::layer::current_caller()?;
    if !caller.unrestricted && caller.backend_roles.is_empty() {
        return Some(err(
            StatusCode::FORBIDDEN,
            "status_exception",
            "User doesn't have backend roles configured. Contact administrator",
        ));
    }
    None
}

/// Whether a job's user may do `action` to these indices, answering with the
/// plugin's words for the refusal: the missing permission and who lacked it.
pub fn permission_refusal(
    store: &Store,
    user: Option<&Value>,
    action: &str,
    indices: &[String],
) -> Option<String> {
    if !store.security.enabled {
        return None;
    }
    let user = user.filter(|u| !u.is_null())?;
    let caller = caller_of(user);
    let cfg = store.security.config.read();
    match cfg.index_verdict(&caller, action, indices) {
        crate::security::Verdict::Allowed | crate::security::Verdict::Partial(_) => None,
        _ => Some(format!("no permissions for [{action}] and {}", caller.describe())),
    }
}

/// Run `what` as the job's user, so that searches it makes see what that user
/// sees -- document- and field-level rules included.
pub fn run_as<T>(user: Option<&Value>, what: impl FnOnce() -> T) -> T {
    match user.filter(|u| !u.is_null()) {
        Some(user) => crate::security::layer::CALLER.sync_scope(caller_of(user), what),
        None => what(),
    }
}

// ------------------------------------------------------------------ scheduling

/// When each job last ran and is next due, by kind and id, with the enabled
/// time it was worked out from: a job started again gets a new time and is
/// scheduled afresh.
fn due_table() -> &'static Mutex<HashMap<String, (i64, i64)>> {
    static TABLE: OnceLock<Mutex<HashMap<String, (i64, i64)>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether a job whose schedule and enabled time are these is due now, moving
/// its next time on when it is.
pub fn due(kind: &str, id: &str, schedule: &Value, enabled_at: i64, now: i64) -> bool {
    let key = format!("{kind}:{id}");
    let mut table = due_table().lock();
    let entry = table.get(&key).copied();
    let next = match entry {
        Some((marker, next)) if marker == enabled_at => next,
        _ => {
            let Some(next) = next_run(schedule, now.max(enabled_at)) else { return false };
            table.insert(key.clone(), (enabled_at, next));
            next
        }
    };
    if now < next {
        return false;
    }
    if let Some(after) = next_run(schedule, now) {
        table.insert(key, (enabled_at, after));
    }
    true
}

/// When a job is next due, where it has been scheduled at all: a job nothing
/// has looked at yet has no place in the table and no time to report.
pub fn next_due(kind: &str, id: &str) -> Option<i64> {
    due_table().lock().get(&format!("{kind}:{id}")).map(|(_, next)| *next)
}

/// Forget a job's place in the schedule, when it is deleted.
pub fn forget(kind: &str, id: &str) {
    due_table().lock().remove(&format!("{kind}:{id}"));
}

/// Look at every transform and rollup once, and run the ones that are due.
pub fn tick(store: &Store) {
    let now = crate::store::now_millis();
    for (id, held) in all(store, "transform") {
        let job = &held.body["transform"];
        if job.get("enabled").and_then(|v| v.as_bool()) != Some(true) {
            continue;
        }
        let enabled_at = job.get("enabled_at").and_then(|v| v.as_i64()).unwrap_or(0);
        if due("transform", &id, &job["schedule"], enabled_at, now) {
            super::transform::run(store, &id);
        }
    }
    if store
        .cluster_setting("plugins.rollup.enabled")
        .and_then(|v| v.as_bool().or_else(|| v.as_str().map(|s| s != "false")))
        == Some(false)
    {
        return;
    }
    for (id, held) in all(store, "rollup") {
        let job = &held.body["rollup"];
        if job.get("enabled").and_then(|v| v.as_bool()) != Some(true) {
            continue;
        }
        let enabled_at = job.get("enabled_time").and_then(|v| v.as_i64()).unwrap_or(0);
        if due("rollup", &id, &job["schedule"], enabled_at, now) {
            super::rollup::run(store, &id);
        }
    }
}

// ------------------------------------------------------------------ searching

/// Run a search as the node would answer it, handing back the answer or the
/// reason it was refused.
pub fn search(store: &Store, index: &str, body: &Value) -> Result<Value, String> {
    let params = crate::api::Params::new();
    match crate::search::run(store, index, body, &params) {
        Ok(out) => {
            let env = crate::search::envelope(out, body, &params);
            // a shard that failed is a search that failed, for a job: what it
            // wrote would be missing that shard's documents
            if let Some(reason) =
                env.pointer("/_shards/failures/0/reason/reason").and_then(|r| r.as_str())
            {
                return Err(reason.to_string());
            }
            Ok(env)
        }
        Err(response) => Err(response
            .extensions()
            .get::<crate::api::shared::ErrorKind>()
            .map(|e| e.reason.clone())
            .unwrap_or_else(|| format!("search failed with status {}", response.status()))),
    }
}

/// Every bucket a composite aggregation has over an index, in key order.
///
/// A job pages through its buckets a page at a time, and the plugin counts
/// what it did in those pages. The pages are counted by the caller from the
/// whole list: asking for them one at a time here would work every bucket out
/// again for each page.
pub fn all_buckets(
    store: &Store,
    index: &str,
    query: &Value,
    sources: &Value,
    aggs: &Value,
) -> Result<(Vec<Value>, u64), String> {
    const CHUNK: usize = 10_000;
    let started = std::time::Instant::now();
    let mut out: Vec<Value> = Vec::new();
    let mut after: Option<Value> = None;
    loop {
        let mut composite = json!({"size": CHUNK, "sources": sources});
        if let Some(a) = &after {
            composite["after"] = a.clone();
        }
        let mut agg = json!({"composite": composite});
        if aggs.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
            agg["aggs"] = aggs.clone();
        }
        let body = json!({"size": 0, "query": query, "aggs": {"buckets": agg}});
        let answer = search(store, index, &body)?;
        let buckets = answer
            .pointer("/aggregations/buckets/buckets")
            .and_then(|b| b.as_array())
            .cloned()
            .unwrap_or_default();
        let n = buckets.len();
        after = answer.pointer("/aggregations/buckets/after_key").cloned();
        out.extend(buckets);
        if n < CHUNK || after.is_none() {
            break;
        }
    }
    Ok((out, started.elapsed().as_millis() as u64))
}

/// Write documents into an index under the ids given, creating nothing: the
/// index is there by the time this is called. Answers with how long it took.
pub fn index_docs(store: &Store, index: &str, docs: &[(String, Value)]) -> Result<u64, String> {
    if docs.is_empty() {
        return Ok(0);
    }
    let started = std::time::Instant::now();
    let Some(st) = store.get(index) else {
        return Err(format!("no such index [{index}]"));
    };
    let mut g = st.write();
    for (id, doc) in docs {
        if let Err(r) = crate::api::write_doc_internal(&mut g, id, doc.clone(), "index", None, None)
        {
            let reason = r
                .extensions()
                .get::<crate::api::shared::ErrorKind>()
                .map(|e| e.reason.clone())
                .unwrap_or_else(|| "the document was refused".into());
            return Err(reason);
        }
    }
    Ok(started.elapsed().as_millis() as u64)
}

/// Make what a job wrote searchable.
pub fn refresh(store: &Store, index: &str) {
    if let Some(st) = store.get(index) {
        let _ = st.write().refresh();
    }
}

/// The type a field has in an index's mapping.
pub fn field_type(store: &Store, index: &str, field: &str) -> Option<String> {
    let st = store.get(index)?;
    let g = st.read();
    g.mapping.type_of(field).map(String::from)
}

/// A double as the plugin writes it into a document: a number where it is
/// one, and the JVM's name for it where it is not.
pub fn double_value(f: f64) -> Value {
    if f.is_nan() {
        json!("NaN")
    } else if f.is_infinite() {
        json!(if f > 0.0 { "Infinity" } else { "-Infinity" })
    } else {
        json!(f)
    }
}

// -------------------------------------------------------------------- queries

/// A query as the plugin writes it back: the way OpenSearch's query builders
/// print themselves, every default spelled out. A job's query is stored and
/// shown in that form, so a query read back is the query that runs.
pub fn canonical_query(q: &Value) -> Result<Value, (String, String)> {
    let unknown = |name: &str| ("parsing_exception".to_string(), format!("unknown query [{name}]"));
    let Some((kind, body)) = q.as_object().and_then(|o| o.iter().next()) else {
        return Err((
            "parsing_exception".into(),
            "Failed to parse object: expecting token of type [FIELD_NAME] but found [END_OBJECT]"
                .into(),
        ));
    };
    let boost = |v: Option<&Value>| json!(v.and_then(|b| b.as_f64()).unwrap_or(1.0));
    // a query naming one field: `{"term": {"f": v}}` or `{"term": {"f": {...}}}`
    let one_field = |body: &Value| -> Option<(String, Value)> {
        body.as_object()?
            .iter()
            .find(|(k, _)| !matches!(k.as_str(), "boost" | "_name"))
            .map(|(k, v)| (k.clone(), v.clone()))
    };
    let out = match kind.as_str() {
        "match_all" | "match_none" => json!({kind: {"boost": boost(body.get("boost"))}}),
        "term" | "prefix" | "wildcard" | "regexp" => {
            let Some((field, v)) = one_field(body) else { return Err(unknown(kind)) };
            let value_key = if kind == "wildcard" { "wildcard" } else { "value" };
            let (value, extra) = match v {
                Value::Object(o) => {
                    let value =
                        o.get("value").or_else(|| o.get(value_key)).cloned().unwrap_or(Value::Null);
                    (value, o)
                }
                other => (other, serde_json::Map::new()),
            };
            let mut spelled = serde_json::Map::new();
            spelled.insert(value_key.into(), value);
            for (k, v) in &extra {
                if !matches!(k.as_str(), "value" | "wildcard" | "boost") {
                    spelled.insert(k.clone(), v.clone());
                }
            }
            if kind == "regexp" {
                spelled.entry("flags_value").or_insert(json!(65535));
                spelled.entry("max_determinized_states").or_insert(json!(10000));
            }
            spelled.insert("boost".into(), boost(extra.get("boost")));
            json!({kind: {field: spelled}})
        }
        "terms" => {
            let Some((field, v)) = one_field(body) else { return Err(unknown(kind)) };
            json!({kind: {field: v, "boost": boost(body.get("boost"))}})
        }
        "exists" => json!({kind: {"field": body.get("field"), "boost": boost(body.get("boost"))}}),
        "ids" => json!({kind: {"values": body.get("values").cloned().unwrap_or(json!([])),
            "boost": boost(body.get("boost"))}}),
        "range" => {
            let Some((field, v)) = one_field(body) else { return Err(unknown(kind)) };
            let o = v.as_object().cloned().unwrap_or_default();
            let (from, include_lower) = match (o.get("gte"), o.get("gt"), o.get("from")) {
                (Some(v), _, _) => (v.clone(), true),
                (None, Some(v), _) => (v.clone(), false),
                (None, None, Some(v)) => {
                    (v.clone(), o.get("include_lower").and_then(|b| b.as_bool()).unwrap_or(true))
                }
                _ => (Value::Null, true),
            };
            let (to, include_upper) = match (o.get("lte"), o.get("lt"), o.get("to")) {
                (Some(v), _, _) => (v.clone(), true),
                (None, Some(v), _) => (v.clone(), false),
                (None, None, Some(v)) => {
                    (v.clone(), o.get("include_upper").and_then(|b| b.as_bool()).unwrap_or(true))
                }
                _ => (Value::Null, true),
            };
            let mut spelled = json!({"from": from, "to": to, "include_lower": include_lower,
                "include_upper": include_upper});
            for key in ["time_zone", "format", "relation"] {
                if let Some(v) = o.get(key) {
                    spelled[key] = v.clone();
                }
            }
            spelled["boost"] = boost(o.get("boost"));
            json!({kind: {field: spelled}})
        }
        "match" => {
            let Some((field, v)) = one_field(body) else { return Err(unknown(kind)) };
            let o = match v {
                Value::Object(o) => o,
                other => serde_json::Map::from_iter([("query".to_string(), other)]),
            };
            let mut spelled = json!({
                "query": o.get("query"),
                "operator": o.get("operator").and_then(|v| v.as_str())
                    .map(|s| s.to_ascii_uppercase()).unwrap_or_else(|| "OR".into()),
            });
            for key in ["analyzer", "fuzziness", "minimum_should_match"] {
                if let Some(v) = o.get(key) {
                    spelled[key] = v.clone();
                }
            }
            spelled["prefix_length"] = o.get("prefix_length").cloned().unwrap_or(json!(0));
            spelled["max_expansions"] = o.get("max_expansions").cloned().unwrap_or(json!(50));
            spelled["fuzzy_transpositions"] =
                o.get("fuzzy_transpositions").cloned().unwrap_or(json!(true));
            spelled["lenient"] = o.get("lenient").cloned().unwrap_or(json!(false));
            spelled["zero_terms_query"] = json!(
                o.get("zero_terms_query")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_ascii_uppercase())
                    .unwrap_or_else(|| "NONE".into())
            );
            spelled["auto_generate_synonyms_phrase_query"] =
                o.get("auto_generate_synonyms_phrase_query").cloned().unwrap_or(json!(true));
            spelled["boost"] = boost(o.get("boost"));
            json!({kind: {field: spelled}})
        }
        "match_phrase" | "match_phrase_prefix" => {
            let Some((field, v)) = one_field(body) else { return Err(unknown(kind)) };
            let o = match v {
                Value::Object(o) => o,
                other => serde_json::Map::from_iter([("query".to_string(), other)]),
            };
            let mut spelled = json!({"query": o.get("query")});
            if let Some(a) = o.get("analyzer") {
                spelled["analyzer"] = a.clone();
            }
            spelled["slop"] = o.get("slop").cloned().unwrap_or(json!(0));
            if kind == "match_phrase_prefix" {
                spelled["max_expansions"] = o.get("max_expansions").cloned().unwrap_or(json!(50));
            } else {
                spelled["zero_terms_query"] = json!(
                    o.get("zero_terms_query")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_ascii_uppercase())
                        .unwrap_or_else(|| "NONE".into())
                );
            }
            spelled["boost"] = boost(o.get("boost"));
            json!({kind: {field: spelled}})
        }
        "query_string" => {
            let o = body.as_object().cloned().unwrap_or_default();
            let mut spelled = json!({"query": o.get("query")});
            if let Some(v) = o.get("default_field") {
                spelled["default_field"] = v.clone();
            }
            spelled["fields"] = o.get("fields").cloned().unwrap_or(json!([]));
            spelled["type"] = o.get("type").cloned().unwrap_or(json!("best_fields"));
            spelled["default_operator"] = json!(
                o.get("default_operator")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_ascii_lowercase())
                    .unwrap_or_else(|| "or".into())
            );
            spelled["max_determinized_states"] = json!(10000);
            spelled["enable_position_increments"] = json!(true);
            spelled["fuzziness"] = o.get("fuzziness").cloned().unwrap_or(json!("AUTO"));
            spelled["fuzzy_prefix_length"] = json!(0);
            spelled["fuzzy_max_expansions"] = json!(50);
            spelled["phrase_slop"] = o.get("phrase_slop").cloned().unwrap_or(json!(0));
            spelled["escape"] = json!(false);
            spelled["auto_generate_synonyms_phrase_query"] = json!(true);
            spelled["fuzzy_transpositions"] = json!(true);
            spelled["boost"] = boost(o.get("boost"));
            json!({kind: spelled})
        }
        "constant_score" => {
            let inner = body.get("filter").map(canonical_query).transpose()?;
            json!({kind: {"filter": inner, "boost": boost(body.get("boost"))}})
        }
        "bool" => {
            let o = body.as_object().cloned().unwrap_or_default();
            let mut spelled = serde_json::Map::new();
            for clause in ["must", "filter", "must_not", "should"] {
                let list: Vec<&Value> = match o.get(clause) {
                    Some(Value::Array(a)) => a.iter().collect(),
                    Some(one @ Value::Object(_)) => vec![one],
                    _ => Vec::new(),
                };
                if list.is_empty() {
                    continue;
                }
                let done: Result<Vec<Value>, _> = list.into_iter().map(canonical_query).collect();
                spelled.insert(clause.into(), Value::Array(done?));
            }
            if let Some(m) = o.get("minimum_should_match") {
                spelled
                    .insert("minimum_should_match".into(), json!(m.to_string().trim_matches('"')));
            }
            spelled.insert(
                "adjust_pure_negative".into(),
                o.get("adjust_pure_negative").cloned().unwrap_or(json!(true)),
            );
            spelled.insert("boost".into(), boost(o.get("boost")));
            json!({kind: spelled})
        }
        other if KNOWN_QUERIES.contains(&other) => q.clone(),
        other => return Err(unknown(other)),
    };
    Ok(out)
}

/// The queries a job's query may be written with, beyond those spelled out
/// above; they are kept as written.
const KNOWN_QUERIES: &[&str] = &[
    "fuzzy",
    "multi_match",
    "simple_query_string",
    "nested",
    "dis_max",
    "boosting",
    "function_score",
    "script",
    "geo_distance",
    "geo_bounding_box",
    "geo_shape",
    "has_child",
    "has_parent",
    "match_bool_prefix",
    "combined_fields",
    "terms_set",
    "intervals",
    "wrapper",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_the_plugins() {
        // counted by the plugin against the same keys
        assert_eq!(hash_id("tfix-t1#east:1738368000000"), "-LGeIvpbOlq7nv23bZayug");
        assert_eq!(
            hash_id("tfix-d##ODFE-MAGIC-NULL-MAGIC-ODFE#:1735862400000:2025-01-02:10.0"),
            "JtAiM_iwh2S1F4kPelwDIQ"
        );
        assert_eq!(hash_id("tfix-r1#1735689600000#east#0.0"), "yls1zhr02AyVdcao4JRyLQ");
    }

    #[test]
    fn murmur_matches_the_reference_vectors() {
        assert_eq!(murmur3_x64_128(b"hello", 0), (0xcbd8_a7b3_41bd_9b02, 0x5b1e_906a_48ae_1d19));
        assert_eq!(murmur3_x64_128(b"", 0), (0, 0));
    }

    #[test]
    fn doubles_print_the_java_way() {
        assert_eq!(java_double(5.0), "5.0");
        assert_eq!(java_double(0.0), "0.0");
        assert_eq!(java_double(2.5), "2.5");
        assert_eq!(java_double(1.0e7), "1.0E7");
    }

    #[test]
    fn an_interval_runs_on_the_grid_of_its_start() {
        let s = json!({"interval": {"start_time": 1_000, "period": 1, "unit": "Minutes"}});
        assert_eq!(next_run(&s, 1_000), Some(61_000));
        assert_eq!(next_run(&s, 60_999), Some(61_000));
        assert_eq!(next_run(&s, 61_000), Some(121_000));
        assert_eq!(next_run(&s, 0), Some(1_000));
    }

    #[test]
    fn cron_finds_the_next_matching_minute() {
        let s = json!({"cron": {"expression": "30 2 * * *", "timezone": "UTC"}});
        // 1970-01-01T00:00Z to 02:30 the same day
        assert_eq!(next_run(&s, 0), Some(9_000_000));
        let every_quarter = json!({"cron": {"expression": "*/15 * * * *", "timezone": "UTC"}});
        assert_eq!(next_run(&every_quarter, 60_000), Some(900_000));
    }

    #[test]
    fn queries_are_spelled_out() {
        let q = canonical_query(&json!({"bool": {"must": [{"term": {"a": "x"}}],
            "filter": {"range": {"n": {"gt": 1, "lte": 5}}}}}))
        .unwrap();
        assert_eq!(
            q,
            json!({"bool": {
                "must": [{"term": {"a": {"value": "x", "boost": 1.0}}}],
                "filter": [{"range": {"n": {"from": 1, "to": 5, "include_lower": false,
                    "include_upper": true, "boost": 1.0}}}],
                "adjust_pure_negative": true, "boost": 1.0}})
        );
        assert!(canonical_query(&json!({"bogus": {}})).is_err());
    }
}
