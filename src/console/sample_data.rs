//! The sample data sets.
//!
//! The home page offers three: flights, web logs, e-commerce. Each is an
//! index of a few thousand documents and a set of saved objects drawing
//! them -- an index pattern, some visualizations, a dashboard. The documents
//! are shipped with the distribution as gzipped JSON lines, dated around a
//! fixed day years ago; installing one moves every date so that the data
//! ends today, keeping the day of the week, because the dashboards show
//! weekly patterns.
//!
//! What is code in the server being replaced -- the field mappings, the
//! saved objects, which fields are dates -- is pinned to
//! `console/sample_data.json` by `tools/osd_sample_data.js`; the data is
//! read from the distribution at install time.

use std::io::BufRead;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::engine::{Engine, Failed};
use super::saved::{Saved, Writing};

/// The name of a data set's index, where the set does not name it itself.
fn index_name(set: &Value, data_index: &Value) -> String {
    match data_index.get("indexName").and_then(|v| v.as_str()) {
        Some(name) => name.to_string(),
        None => format!(
            "opensearch_dashboards_sample_data_{}",
            data_index
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| { set.get("id").and_then(|v| v.as_str()).unwrap_or("") })
        ),
    }
}

/// The sets, each with whether it is installed: its indices there and not
/// empty, and its dashboard there.
pub fn list(engine: &Engine, saved: &Saved<'_>, sets: &[Value]) -> Vec<Value> {
    sets.iter()
        .map(|set| {
            let mut out = Map::new();
            for key in [
                "id",
                "name",
                "description",
                "previewImagePath",
                "darkPreviewImagePath",
                "hasNewThemeImages",
                "overviewDashboard",
                "appLinks",
                "defaultIndex",
            ] {
                if let Some(v) = set.get(key) {
                    out.insert(key.into(), v.clone());
                }
            }
            let data_indices: Vec<Value> = set
                .get("dataIndices")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|i| {
                    let mut one = json!({"id": i.get("id")});
                    if let Some(name) = i.get("indexName") {
                        one["indexName"] = name.clone();
                    }
                    one
                })
                .collect();
            out.insert("dataIndices".into(), Value::Array(data_indices));
            let (status, message) = status_of(engine, saved, set);
            out.insert("status".into(), json!(status));
            if let Some(message) = message {
                out.insert("statusMsg".into(), json!(message));
            }
            Value::Object(out)
        })
        .collect()
}

fn status_of(engine: &Engine, saved: &Saved<'_>, set: &Value) -> (&'static str, Option<String>) {
    for data_index in set.get("dataIndices").and_then(|v| v.as_array()).into_iter().flatten() {
        let index = index_name(set, data_index);
        let count = match engine.call("GET", &format!("/{index}/_count"), None) {
            Ok(found) if found.get("error").is_some() => {
                if found.pointer("/error/type").and_then(|v| v.as_str())
                    == Some("index_not_found_exception")
                {
                    return ("not_installed", None);
                }
                return ("unknown", Some(found["error"].to_string()));
            }
            Ok(found) => found.get("count").and_then(|v| v.as_u64()).unwrap_or(0),
            Err(e) => return ("unknown", Some(e.message)),
        };
        if count == 0 {
            return ("not_installed", None);
        }
    }
    if let Some(dashboard) = set.get("overviewDashboard").and_then(|v| v.as_str())
        && !dashboard.is_empty()
    {
        match saved.get("dashboard", dashboard) {
            Ok(_) => {}
            Err(e) if e.status == 404 => return ("not_installed", None),
            Err(e) => return ("unknown", Some(e.message)),
        }
    }
    ("installed", None)
}

/// A set installed: each index remade and filled, its dates moved to end
/// at `now`, and its saved objects written over whatever was there.
pub fn install(
    engine: &Engine,
    saved: &Saved<'_>,
    home: &Path,
    set: &Value,
    now: Option<&str>,
) -> Result<Value, Failed> {
    let today = match now {
        Some(given) if given.len() >= 10 && civil_of(&given[..10]).is_some() => {
            given[..10].to_string()
        }
        _ => today(),
    };
    let mut counts = Map::new();
    for data_index in set.get("dataIndices").and_then(|v| v.as_array()).into_iter().flatten() {
        let index = index_name(set, data_index);
        // whatever was there goes; a set installed twice is installed once
        let _ = engine.call("DELETE", &format!("/{index}"), None);
        let made = engine.call(
            "PUT",
            &format!("/{index}"),
            Some(&json!({
                "settings": {"index": {"number_of_shards": 1, "auto_expand_replicas": "0-1"}},
                "mappings": {"properties": data_index.get("fields").cloned().unwrap_or_else(|| json!({}))},
            })),
        )?;
        if let Some(error) = made.get("error") {
            let status = made.get("status").and_then(|v| v.as_u64()).unwrap_or(500) as u16;
            return Err(Failed::of(
                status,
                format!(
                    "Unable to create sample data index \"{index}\", error: {}",
                    error.get("reason").and_then(|v| v.as_str()).unwrap_or("")
                ),
            ));
        }
        let path = home.join(data_index.get("dataPath").and_then(|v| v.as_str()).unwrap_or(""));
        let count = load(engine, &index, &path, data_index, &today).map_err(|e| {
            Failed::of(
                500,
                format!("sample_data install errors while loading data. Error: {}", e.message),
            )
        })?;
        counts.insert(index, json!(count));
    }
    let objects: Vec<Value> =
        set.get("savedObjects").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let writings: Vec<Writing> = objects
        .iter()
        .map(|o| Writing {
            kind: o.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            id: o.get("id").and_then(|v| v.as_str()).map(String::from),
            attributes: o.get("attributes").cloned().unwrap_or_else(|| json!({})),
            references: o.get("references").cloned().unwrap_or_else(|| json!([])),
            migration_version: o.get("migrationVersion").cloned(),
            overwrite: true,
        })
        .collect();
    let written = saved.bulk_create(writings)?;
    let errors: Vec<String> =
        written.iter().filter_map(|w| w.as_ref().err()).map(|e| e.message.clone()).collect();
    if !errors.is_empty() {
        return Err(Failed::of(
            403,
            format!(
                "sample_data install errors while loading saved objects. Errors: {}",
                errors.join(",")
            ),
        ));
    }
    Ok(json!({
        "opensearchIndicesCreated": counts,
        "opensearchDashboardsSavedObjectsLoaded": objects.len(),
    }))
}

/// The documents read from the distribution's file, dated anew, and put in
/// the index five hundred at a time.
fn load(
    engine: &Engine,
    index: &str,
    path: &Path,
    data_index: &Value,
    today: &str,
) -> Result<u64, Failed> {
    let file = std::fs::File::open(path)
        .map_err(|e| Failed::of(500, format!("{}: {e}", path.display())))?;
    let reader = std::io::BufReader::new(flate2::read::MultiGzDecoder::new(file));
    let time_fields: Vec<String> = data_index
        .get("timeFields")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|f| f.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let marker = data_index.get("currentTimeMarker").and_then(|v| v.as_str()).unwrap_or("");
    let by_week =
        data_index.get("preserveDayOfWeekTimeOfDay").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut count = 0u64;
    let mut lines = String::new();
    let mut in_batch = 0;
    let action = json!({"index": {"_index": index}}).to_string();
    let flush = |lines: &mut String| -> Result<(), Failed> {
        if lines.is_empty() {
            return Ok(());
        }
        let answer = engine.bulk_with("refresh=false", lines)?;
        lines.clear();
        if answer.get("errors").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err(Failed::of(
                500,
                format!(
                    "Unable to load sample data into index \"{index}\", see OpenSearch Dashboards logs for details"
                ),
            ));
        }
        Ok(())
    };
    for line in reader.lines() {
        let line = line.map_err(|e| Failed::of(500, format!("{}: {e}", path.display())))?;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut doc: Value = serde_json::from_str(&line).map_err(|e| {
            Failed::of(
                500,
                format!(
                    "Unable to parse line as JSON document, line: \"\"\"{line}\"\"\", Error: {e}"
                ),
            )
        })?;
        for field in &time_fields {
            if let Some(Value::String(time)) = nested(&doc, field).cloned() {
                let moved = match by_week {
                    true => relative_to_week(&time, marker, today),
                    false => relative_to_difference(&time, marker, today),
                };
                if let Some(moved) = moved {
                    set_nested(&mut doc, field, json!(moved));
                }
            }
        }
        lines.push_str(&action);
        lines.push('\n');
        lines.push_str(&doc.to_string());
        lines.push('\n');
        count += 1;
        in_batch += 1;
        if in_batch >= 500 {
            flush(&mut lines)?;
            in_batch = 0;
        }
    }
    flush(&mut lines)?;
    // the page counts the documents the moment the install answers
    let _ = engine.call("POST", &format!("/{index}/_refresh"), None);
    Ok(count)
}

/// A set uninstalled: its saved objects gone, then its indices.
pub fn uninstall(engine: &Engine, saved: &Saved<'_>, set: &Value) -> Result<(), Failed> {
    for object in set.get("savedObjects").and_then(|v| v.as_array()).into_iter().flatten() {
        let kind = object.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let id = object.get("id").and_then(|v| v.as_str()).unwrap_or("");
        match saved.delete(kind, id) {
            Ok(_) => {}
            Err(e) if e.status == 404 => {}
            Err(e) => {
                return Err(Failed::of(
                    e.status,
                    format!("Unable to delete sample dataset saved objects, error: {}", e.message),
                ));
            }
        }
    }
    for data_index in set.get("dataIndices").and_then(|v| v.as_array()).into_iter().flatten() {
        let index = index_name(set, data_index);
        let found = engine.call("DELETE", &format!("/{index}"), None)?;
        if let Some(error) = found.get("error") {
            let status = found.get("status").and_then(|v| v.as_u64()).unwrap_or(500) as u16;
            let kind = error.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let reason = error.get("reason").and_then(|v| v.as_str()).unwrap_or("");
            return Err(Failed::of(
                status,
                format!("Unable to delete sample data index \"{index}\", error: [{kind}] {reason}"),
            ));
        }
    }
    Ok(())
}

fn nested<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    if let Some(v) = doc.get(path) {
        return Some(v);
    }
    path.split('.').try_fold(doc, |at, key| at.get(key))
}

fn set_nested(doc: &mut Value, path: &str, value: Value) {
    if doc.get(path).is_some() {
        doc[path] = value;
        return;
    }
    let keys: Vec<&str> = path.split('.').collect();
    let mut at = doc;
    for (n, key) in keys.iter().enumerate() {
        if n == keys.len() - 1 {
            at[*key] = value;
            return;
        }
        if at.get(*key).is_none_or(|v| !v.is_object()) {
            at[*key] = json!({});
        }
        at = &mut at[*key];
    }
}

// ---- dates, as days ---------------------------------------------------------

/// Days since 1970-01-01 of a `YYYY-MM-DD`, or nothing where it is not one.
fn civil_of(text: &str) -> Option<i64> {
    let mut parts = text.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.get(..2).unwrap_or("").parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

/// `YYYY-MM-DD` of a day count.
fn date_of(days: i64) -> String {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Sunday is 0, as JavaScript counts.
fn weekday(days: i64) -> i64 {
    (days + 4).rem_euclid(7)
}

fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    date_of(secs.div_euclid(86400))
}

/// The source's time moved by as many days as separate the two references.
fn relative_to_difference(
    source: &str,
    source_reference: &str,
    target_reference: &str,
) -> Option<String> {
    let delta = civil_of(source.get(..10)?)? - civil_of(source_reference.get(..10)?)?;
    let moved = civil_of(target_reference.get(..10)?)? + delta;
    Some(format!("{}T{}", date_of(moved), source.get(11..).unwrap_or("")))
}

/// The same, with the target reference first moved to the source
/// reference's day of the week, so that a Monday stays a Monday.
fn relative_to_week(
    source: &str,
    source_reference: &str,
    target_reference: &str,
) -> Option<String> {
    let source_days = civil_of(source_reference.get(..10)?)?;
    let target_days = civil_of(target_reference.get(..10)?)?;
    let normalized = target_days + (weekday(source_days) - weekday(target_days));
    relative_to_difference(source, source_reference, &date_of(normalized))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_and_dates_round_trip() {
        assert_eq!(civil_of("1970-01-01"), Some(0));
        assert_eq!(date_of(0), "1970-01-01");
        assert_eq!(civil_of("2018-01-09"), Some(17540));
        assert_eq!(date_of(17540), "2018-01-09");
        assert_eq!(weekday(civil_of("2018-01-09").unwrap()), 2); // a Tuesday
        assert_eq!(weekday(0), 4); // a Thursday
        assert_eq!(civil_of("2018-13-01"), None);
    }

    #[test]
    fn times_are_moved_by_the_difference_of_the_references() {
        assert_eq!(
            relative_to_difference("2018-01-08T10:00:00", "2018-01-09T00:00:00", "2026-09-07"),
            Some("2026-09-06T10:00:00".to_string())
        );
    }

    #[test]
    fn the_day_of_the_week_is_kept() {
        // 2018-01-09 is a Tuesday; 2026-09-07 is a Monday, so the reference
        // is moved to Tuesday 2026-09-08 and a source Monday lands on a Monday
        assert_eq!(
            relative_to_week("2018-01-08T10:00:00", "2018-01-09T00:00:00", "2026-09-07"),
            Some("2026-09-07T10:00:00".to_string())
        );
    }

    #[test]
    fn nested_fields_are_read_and_set() {
        let mut doc = json!({"a": {"b": "x"}, "flat.name": "y"});
        assert_eq!(nested(&doc, "a.b"), Some(&json!("x")));
        assert_eq!(nested(&doc, "flat.name"), Some(&json!("y")));
        set_nested(&mut doc, "a.b", json!("z"));
        set_nested(&mut doc, "flat.name", json!("w"));
        set_nested(&mut doc, "c.d", json!(1));
        assert_eq!(doc, json!({"a": {"b": "z"}, "flat.name": "w", "c": {"d": 1}}));
    }
}
