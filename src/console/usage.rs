//! What the front end reports about its own use, and what `/api/stats`
//! says about the server.
//!
//! Two of the pages' routes are counters: the query bar says whether
//! somebody turned DQL on or off, and every page reports which buttons were
//! clicked and for how long it was open. Both are kept as saved objects in
//! the console's index, the way the server being replaced keeps them, so
//! that a console that replaces it counts on from where it left off.

use serde_json::{Map, Value, json};

use super::engine::Failed;
use super::saved::{Saved, Writing};

/// The report the front end sends: which keys it may have.
const REPORT_KEYS: [&str; 4] =
    ["reportVersion", "userAgent", "uiStatsMetrics", "application_usage"];

/// Somebody opted DQL in or out: the matching counter goes up by one.
pub fn dql_opt_in(saved: &Saved<'_>, opt_in: bool) -> Result<Value, Failed> {
    let counter = if opt_in { "optInCount" } else { "optOutCount" };
    saved.increment_counter("dql-telemetry", "dql-telemetry", counter, 1).map_err(|e| {
        Failed::of(e.status, "Something went wrong").with_attributes(json!({"success": false}))
    })?;
    Ok(json!({"success": true}))
}

/// A page's report stored: one object per user agent, a counter per event,
/// and a row per application that was open.
///
/// The server being replaced batches these and writes them once a minute;
/// this writes them as they come, which is what its own suite expected of
/// it and it did not do.
pub fn store_report(saved: &Saved<'_>, report: &Value) -> Result<(), Failed> {
    let report = report.as_object().ok_or_else(|| {
        Failed::of(
            400,
            "[request body.report]: expected value of type [object] but got [undefined]",
        )
    })?;
    for key in report.keys() {
        if !REPORT_KEYS.contains(&key.as_str()) {
            return Err(Failed::of(
                400,
                format!("[request body.report.{key}]: definition for this key is missing"),
            ));
        }
    }
    if let Some(agents) = report.get("userAgent").and_then(|v| v.as_object()) {
        for (key, metric) in agents {
            let agent = metric.get("userAgent").and_then(|v| v.as_str()).ok_or_else(|| {
                Failed::of(400, format!("[request body.report.userAgent.{key}.userAgent]: expected value of type [string] but got [undefined]"))
            })?;
            saved.create(Writing {
                kind: "ui-metric".to_string(),
                id: Some(format!("{key}:{agent}")),
                attributes: json!({"count": 1}),
                references: json!([]),
                migration_version: None,
                overwrite: true,
            })?;
        }
    }
    if let Some(metrics) = report.get("uiStatsMetrics").and_then(|v| v.as_object()) {
        for (key, metric) in metrics {
            let field = |name: &str| {
                metric.get(name).and_then(|v| v.as_str()).map(String::from).ok_or_else(|| {
                    Failed::of(400, format!("[request body.report.uiStatsMetrics.{key}.{name}]: expected value of type [string] but got [undefined]"))
                })
            };
            let app = field("appName")?;
            let event = field("eventName")?;
            let sum = metric.pointer("/stats/sum").and_then(|v| v.as_f64()).ok_or_else(|| {
                Failed::of(400, format!("[request body.report.uiStatsMetrics.{key}.stats.sum]: expected value of type [number] but got [undefined]"))
            })?;
            saved.increment_counter("ui-metric", &format!("{app}:{event}"), "count", sum as i64)?;
        }
    }
    if let Some(usage) = report.get("application_usage").and_then(|v| v.as_object()) {
        let now = super::now();
        let writings: Vec<Writing> = usage
            .iter()
            .map(|(app, metric)| Writing {
                kind: "application_usage_transactional".to_string(),
                id: None,
                attributes: json!({
                    "appId": app,
                    "minutesOnScreen": metric.get("minutesOnScreen").cloned().unwrap_or(json!(0)),
                    "numberOfClicks": metric.get("numberOfClicks").cloned().unwrap_or(json!(0)),
                    "timestamp": now,
                }),
                references: json!([]),
                migration_version: None,
                overwrite: false,
            })
            .collect();
        if !writings.is_empty() {
            for answer in saved.bulk_create(writings)? {
                answer?;
            }
        }
    }
    Ok(())
}

/// Names as `/api/stats` spells them: snake case, `_bytes` rather than
/// `_in_bytes`, `_ms` rather than `_in_millis`, and the load averages as
/// `1m` rather than `1_m`.
pub fn api_field_names(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let name =
                        snake_case(&k).replace("_in_bytes", "_bytes").replace("_in_millis", "_ms");
                    let name = match name.as_str() {
                        "1_m" => "1m".to_string(),
                        "5_m" => "5m".to_string(),
                        "15_m" => "15m".to_string(),
                        _ => name,
                    };
                    (name, api_field_names(v))
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(api_field_names).collect()),
        other => other,
    }
}

fn snake_case(key: &str) -> String {
    let mut out = String::new();
    let mut previous_lower = false;
    for ch in key.chars() {
        if ch.is_ascii_uppercase() {
            if previous_lower {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
            previous_lower = false;
        } else if ch == '-' || ch == ' ' {
            out.push('_');
            previous_lower = false;
        } else {
            previous_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
            out.push(ch);
        }
    }
    out
}

/// What the server has been used for, as `/api/stats?extended` reports it
/// in place of the collectors the server being replaced runs: how many of
/// each object there are, the DQL counters, the event counters.
pub fn usage(saved: &Saved<'_>) -> Value {
    let mut out = Map::new();
    let count = |kind: &str| -> Value {
        let looking = super::saved::Looking {
            types: vec![kind.to_string()],
            per_page: 0,
            ..Default::default()
        };
        json!({"total": saved.find(&looking).ok().and_then(|f| f.get("total").cloned()).unwrap_or(json!(0))})
    };
    out.insert(
        "opensearchDashboards".into(),
        json!({
            "index": super::engine::INDEX,
            "dashboard": count("dashboard"),
            "visualization": count("visualization"),
            "search": count("search"),
            "index_pattern": count("index-pattern"),
            "graph_workspace": {"total": 0},
        }),
    );
    let dql = saved.get("dql-telemetry", "dql-telemetry").ok();
    out.insert(
        "dql".into(),
        json!({
            "optInCount": dql.as_ref().and_then(|d| d.pointer("/attributes/optInCount").cloned()).unwrap_or(json!(0)),
            "optOutCount": dql.as_ref().and_then(|d| d.pointer("/attributes/optOutCount").cloned()).unwrap_or(json!(0)),
            "defaultQueryLanguage": "default-kuery",
        }),
    );
    let looking = super::saved::Looking {
        types: vec!["ui-metric".to_string()],
        per_page: 10_000,
        ..Default::default()
    };
    let mut by_app: Map<String, Value> = Map::new();
    for one in saved
        .find(&looking)
        .ok()
        .and_then(|f| f.get("saved_objects").and_then(|v| v.as_array()).cloned())
        .unwrap_or_default()
    {
        let id = one.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let Some((app, event)) = id.split_once(':') else { continue };
        let entry = by_app.entry(app.to_string()).or_insert_with(|| json!([]));
        if let Some(list) = entry.as_array_mut() {
            list.push(json!({"key": event, "value": one.pointer("/attributes/count").cloned().unwrap_or(json!(0))}));
        }
    }
    out.insert("ui_metric".into(), Value::Object(by_app));
    out.insert("application_usage".into(), json!({}));
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_spelled_as_the_stats_api_spells_them() {
        let named = api_field_names(json!({
            "collection_interval_in_millis": 1,
            "process": {"memory": {"heap": {"total_in_bytes": 2}}, "uptime_in_millis": 3},
            "os": {"load": {"1m": 0.5, "15m": 0.2}, "platformRelease": "x"},
            "opensearchDashboards": {"transport_address": "a"},
        }));
        assert_eq!(named["collection_interval_ms"], 1);
        assert_eq!(named["process"]["memory"]["heap"]["total_bytes"], 2);
        assert_eq!(named["process"]["uptime_ms"], 3);
        assert_eq!(named["os"]["load"]["1m"], 0.5);
        assert_eq!(named["os"]["load"]["15m"], 0.2);
        assert_eq!(named["os"]["platform_release"], "x");
        assert_eq!(named["opensearch_dashboards"]["transport_address"], "a");
    }
}
