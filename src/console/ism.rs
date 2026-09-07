//! The Index Management plugin's server half.
//!
//! The page that lists indices is a plugin with a server of its own, and
//! that server is a thin thing: it asks the engine what the page asked it
//! and hands the answer back under `{ok, response}`. Most of what it does
//! goes through one route, `apiCaller`, which takes the name of a call in
//! the old client's vocabulary -- `cat.indices`, `indices.putSettings` --
//! and makes the request that name stands for. This is that vocabulary,
//! and the one route with logic of its own, the index listing.

use serde_json::{Map, Value, json};

use super::engine::{Engine, Failed};

/// `{ok: true, response}`, the shape every answer of this plugin takes.
fn ok(response: Value) -> Value {
    json!({"ok": true, "response": response})
}

/// `{ok: false, error}`: the plugin never answers with a status other than
/// 200, and says what went wrong in the body.
fn not_ok(error: impl Into<String>, body: Value) -> Value {
    json!({"ok": false, "error": error.into(), "body": body})
}

/// The engine's answer to one call, as the old client would have returned
/// it: the body, or the error it raised.
fn call(engine: &Engine, method: &str, path: &str, body: Option<&Value>) -> Result<Value, Failed> {
    let bytes = body.map(|b| b.to_string()).unwrap_or_default();
    let answer = engine.raw(method, path, bytes.as_bytes(), "application/json")?;
    let found: Value = match serde_json::from_slice(&answer.body) {
        Ok(v) => v,
        Err(_) => json!(String::from_utf8_lossy(&answer.body)),
    };
    if answer.status >= 300 {
        let kind = found.pointer("/error/type").and_then(|v| v.as_str()).unwrap_or("");
        let reason = found.pointer("/error/reason").and_then(|v| v.as_str()).unwrap_or("");
        let message = match kind.is_empty() {
            true => format!("Response Error: {}", answer.status),
            false => format!("[{kind}] {reason}"),
        };
        return Err(Failed::of(answer.status, message).with_error(found));
    }
    Ok(found)
}

/// A path segment from a parameter that may be a string or a list.
fn segment(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Array(items) => {
            let joined = items.iter().filter_map(|i| i.as_str()).collect::<Vec<_>>().join(",");
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    }
}

fn encoded(text: &str) -> String {
    percent_encoding::utf8_percent_encode(text, percent_encoding::NON_ALPHANUMERIC)
        .to_string()
        .replace("%2C", ",")
        .replace("%2A", "*")
        .replace("%2D", "-")
        .replace("%2E", ".")
        .replace("%5F", "_")
}

/// The query string from what is left of the parameters once the path has
/// taken its own.
fn query_of(params: &Map<String, Value>, taken: &[&str]) -> String {
    let mut pairs: Vec<String> = Vec::new();
    for (key, value) in params {
        if taken.contains(&key.as_str()) || key == "body" {
            continue;
        }
        let text = match value {
            Value::String(s) => s.clone(),
            Value::Array(items) => items
                .iter()
                .map(|i| i.as_str().map(String::from).unwrap_or_else(|| i.to_string()))
                .collect::<Vec<_>>()
                .join(","),
            Value::Null => continue,
            other => other.to_string(),
        };
        pairs.push(format!(
            "{key}={}",
            form_urlencoded::byte_serialize(text.as_bytes()).collect::<String>()
        ));
    }
    match pairs.is_empty() {
        true => String::new(),
        false => format!("?{}", pairs.join("&")),
    }
}

/// One call by the old client's name.
pub fn api_caller(engine: &Engine, endpoint: &str, data: &Value) -> Value {
    let params = data.as_object().cloned().unwrap_or_default();
    let body = params.get("body");
    let at = |key: &str| segment(params.get(key));
    let joined = |key: &str| at(key).map(|s| encoded(&s)).unwrap_or_default();
    let with = |base: String, key: &str| match at(key) {
        Some(s) => format!("{base}/{}", encoded(&s)),
        None => base,
    };
    let (method, path, taken): (&str, String, Vec<&str>) = match endpoint {
        "transport.request" => {
            let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let path = format!("/{}", path.trim_start_matches('/'));
            let method =
                params.get("method").and_then(|v| v.as_str()).unwrap_or("GET").to_ascii_uppercase();
            if !matches!(method.as_str(), "HEAD" | "GET" | "POST" | "PUT" | "DELETE") {
                return not_ok(
                    format!(
                        "Method must be one of, case insensitive ['HEAD', 'GET', 'POST', 'PUT', 'DELETE']. Received '{}'.",
                        params.get("method").and_then(|v| v.as_str()).unwrap_or("")
                    ),
                    json!(""),
                );
            }
            let query = params
                .get("querystring")
                .and_then(|v| v.as_object())
                .map(|q| query_of(q, &[]))
                .unwrap_or_default();
            let path = match (path.contains('?'), query.is_empty()) {
                (_, true) => path,
                (true, false) => format!("{path}&{}", &query[1..]),
                (false, false) => format!("{path}{query}"),
            };
            return match call(engine, &method, &path, body) {
                Ok(found) => ok(found),
                Err(e) => not_ok(e.message, e.error.map(|b| *b).unwrap_or(json!(""))),
            };
        }
        "cat.indices" => ("GET", with("/_cat/indices".into(), "index"), vec!["index"]),
        "cat.aliases" => ("GET", with("/_cat/aliases".into(), "name"), vec!["name"]),
        "cat.templates" => ("GET", with("/_cat/templates".into(), "name"), vec!["name"]),
        "cat.recovery" => ("GET", with("/_cat/recovery".into(), "index"), vec!["index"]),
        "cat.tasks" => ("GET", "/_cat/tasks".into(), vec![]),
        "cat.shards" => ("GET", with("/_cat/shards".into(), "index"), vec!["index"]),
        "indices.get" => ("GET", format!("/{}", joined("index")), vec!["index"]),
        "indices.exists" => ("HEAD", format!("/{}", joined("index")), vec!["index"]),
        "indices.getSettings" => {
            ("GET", with(format!("/{}/_settings", joined("index")), "name"), vec!["index", "name"])
        }
        "indices.putSettings" => ("PUT", format!("/{}/_settings", joined("index")), vec!["index"]),
        "indices.getMapping" => ("GET", format!("/{}/_mapping", joined("index")), vec!["index"]),
        "indices.putMapping" => ("PUT", format!("/{}/_mapping", joined("index")), vec!["index"]),
        "indices.getFieldMapping" => (
            "GET",
            format!("/{}/_mapping/field/{}", joined("index"), joined("fields")),
            vec!["index", "fields"],
        ),
        "indices.updateAliases" => ("POST", "/_aliases".into(), vec![]),
        "indices.getAlias" => {
            ("GET", with(format!("/{}/_alias", joined("index")), "name"), vec!["index", "name"])
        }
        "indices.putAlias" => (
            "PUT",
            format!("/{}/_alias/{}", joined("index"), joined("name")),
            vec!["index", "name"],
        ),
        "indices.deleteAlias" => (
            "DELETE",
            format!("/{}/_alias/{}", joined("index"), joined("name")),
            vec!["index", "name"],
        ),
        "indices.rollover" => (
            "POST",
            with(format!("/{}/_rollover", joined("alias")), "newIndex"),
            vec!["alias", "newIndex", "new_index"],
        ),
        "indices.validateQuery" => {
            ("POST", format!("/{}/_validate/query", joined("index")), vec!["index"])
        }
        "indices.refresh" => ("POST", format!("/{}/_refresh", joined("index")), vec!["index"]),
        "indices.flush" => ("POST", format!("/{}/_flush", joined("index")), vec!["index"]),
        "indices.clearCache" => {
            ("POST", format!("/{}/_cache/clear", joined("index")), vec!["index"])
        }
        "indices.forcemerge" => {
            ("POST", format!("/{}/_forcemerge", joined("index")), vec!["index"])
        }
        "indices.delete" => ("DELETE", format!("/{}", joined("index")), vec!["index"]),
        "indices.create" => ("PUT", format!("/{}", joined("index")), vec!["index"]),
        "indices.open" => ("POST", format!("/{}/_open", joined("index")), vec!["index"]),
        "indices.close" => ("POST", format!("/{}/_close", joined("index")), vec!["index"]),
        "indices.stats" => ("GET", format!("/{}/_stats", joined("index")), vec!["index"]),
        "indices.getTemplate" => ("GET", with("/_template".into(), "name"), vec!["name"]),
        "indices.getIndexTemplate" => {
            ("GET", with("/_index_template".into(), "name"), vec!["name"])
        }
        "ingest.getPipeline" => ("GET", with("/_ingest/pipeline".into(), "id"), vec!["id"]),
        "cluster.state" => (
            "GET",
            with(with("/_cluster/state".into(), "metric"), "index"),
            vec!["metric", "index"],
        ),
        "cluster.health" => ("GET", with("/_cluster/health".into(), "index"), vec!["index"]),
        "cluster.getSettings" => ("GET", "/_cluster/settings".into(), vec![]),
        "cluster.putSettings" => ("PUT", "/_cluster/settings".into(), vec![]),
        "ism.explain" => ("GET", with("/_plugins/_ism/explain".into(), "index"), vec!["index"]),
        "ism.getPolicy" => {
            ("GET", with("/_plugins/_ism/policies".into(), "policyId"), vec!["policyId"])
        }
        "ism.getPolicies" => ("GET", "/_plugins/_ism/policies".into(), vec![]),
        "ism.add" => ("POST", format!("/_plugins/_ism/add/{}", joined("index")), vec!["index"]),
        "ism.remove" => {
            ("POST", format!("/_plugins/_ism/remove/{}", joined("index")), vec!["index"])
        }
        other => return not_ok(format!("Unknown endpoint: {other}"), json!("")),
    };
    let path = format!("{path}{}", query_of(&params, &taken));
    match call(engine, method, &path, body) {
        Ok(found) => ok(found),
        Err(e) => not_ok(e.message, e.error.map(|b| *b).unwrap_or(json!(""))),
    }
}

/// The index listing: `_cat/indices` for what the page asked, marked with
/// what is being reindexed or recovered, without the data streams' own
/// indices unless asked, paged here because `_cat` does not page, and each
/// index said to be managed by a policy or not.
pub fn indices(engine: &Engine, query: &[(String, String)]) -> Value {
    let value = |key: &str| query.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    let values = |key: &str| -> Vec<String> {
        query
            .iter()
            .filter(|(k, _)| k == key || *k == format!("{key}[]"))
            .map(|(_, v)| v.clone())
            .collect()
    };
    let wrap = |list: Vec<String>| -> String {
        match list.is_empty() {
            true => String::new(),
            false => format!("*{}*", list.join("*,*")),
        }
    };
    let show_data_streams = value("showDataStreams").is_some_and(|v| v == "true");
    let mut index = [wrap(values("terms")), wrap(values("indices")), wrap(values("dataStreams"))]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(",");
    if index.is_empty() {
        index = "*".to_string();
    }
    if !show_data_streams {
        index.push_str(",-.ds*");
    }
    if let Some(exact) = value("exactSearch") {
        index = exact;
    }
    let sort_field = value("sortField").unwrap_or_else(|| "index".into());
    let sort_direction = value("sortDirection").unwrap_or_else(|| "desc".into());
    let mut path = format!("/_cat/indices/{}?format=json", encoded(&index));
    if sort_field != "managed" && sort_field != "data_stream" {
        path.push_str(&format!("&s={sort_field}:{sort_direction}"));
    }
    if let Some(expand) = value("expandWildcards") {
        path.push_str(&format!("&expand_wildcards={expand}"));
    }
    let listed = match call(engine, "GET", &path, None) {
        Ok(Value::Array(rows)) => rows,
        Ok(_) => Vec::new(),
        Err(e) if e.status == 404 => {
            return ok(json!({"indices": [], "totalIndices": 0}));
        }
        Err(e) => return json!({"ok": false, "error": e.message}),
    };
    let recoveries = call(engine, "GET", "/_cat/recovery?format=json&detailed=true", None)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let tasks = call(
        engine,
        "GET",
        "/_cat/tasks?format=json&detailed=true&actions=indices:data/write/reindex",
        None,
    )
    .ok()
    .and_then(|v| v.as_array().cloned())
    .unwrap_or_default();
    let data_streams = call(engine, "GET", "/_data_stream/*", None)
        .ok()
        .and_then(|v| v.get("data_streams").and_then(|d| d.as_array()).cloned())
        .unwrap_or_default();
    let mut rows: Vec<Value> = listed
        .into_iter()
        .map(|mut row| {
            let name = row.get("index").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let stream = data_streams.iter().find(|ds| {
                ds.get("indices").and_then(|i| i.as_array()).is_some_and(|i| {
                    i.iter().any(|x| x.get("index_name").and_then(|v| v.as_str()) == Some(&name))
                })
            });
            row["data_stream"] =
                stream.and_then(|ds| ds.get("name").cloned()).unwrap_or(Value::Null);
            let mut extra = row.get("status").cloned().unwrap_or(Value::Null);
            if row.get("health").and_then(|v| v.as_str()) == Some("green") {
                if tasks.iter().any(|t| {
                    t.get("description")
                        .and_then(|v| v.as_str())
                        .is_some_and(|d| d.contains(&format!("to [{name}]")))
                }) {
                    extra = json!("reindex");
                }
            } else if recoveries.iter().any(|r| {
                r.get("index").and_then(|v| v.as_str()) == Some(&name)
                    && r.get("stage").and_then(|v| v.as_str()) != Some("done")
            }) {
                extra = json!("recovery");
            }
            if !extra.is_null() {
                row["extraStatus"] = extra;
            }
            row
        })
        .collect();
    let text =
        |row: &Value, key: &str| row.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string();
    if sort_field == "status" {
        rows.sort_by(|a, b| match sort_direction.as_str() {
            "asc" => text(a, "extraStatus").cmp(&text(b, "extraStatus")),
            _ => text(b, "extraStatus").cmp(&text(a, "extraStatus")),
        });
    }
    let filtered: Vec<Value> = match show_data_streams {
        true => rows,
        false => {
            rows.into_iter().filter(|r| r.get("data_stream").is_none_or(|v| v.is_null())).collect()
        }
    };
    let total = filtered.len();
    let from: usize = value("from").and_then(|v| v.parse().ok()).unwrap_or(0);
    let size: usize = value("size").and_then(|v| v.parse().ok()).unwrap_or(20);
    let page: Vec<Value> = filtered.into_iter().skip(from).take(size).collect();
    let names: Vec<String> = page.iter().map(|r| text(r, "index")).collect();
    let managed: Map<String, Value> = match names.is_empty() {
        true => Map::new(),
        false => match call(
            engine,
            "GET",
            &format!("/_plugins/_ism/explain/{}", encoded(&names.join(","))),
            None,
        ) {
            Ok(explain) => explain
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter(|(k, _)| *k != "total_managed_indices")
                        .map(|(k, v)| {
                            let policy = v
                                .get("index.plugins.index_state_management.policy_id")
                                .cloned()
                                .unwrap_or(Value::Null);
                            (k.clone(), if policy.is_null() { json!("") } else { policy })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            Err(_) => names.iter().map(|n| (n.clone(), json!("N/A"))).collect(),
        },
    };
    let mut page: Vec<Value> = page
        .into_iter()
        .map(|mut row| {
            let name = text(&row, "index");
            let policy = managed.get(&name).cloned().unwrap_or(json!(""));
            let is_managed = policy.as_str().is_some_and(|p| !p.is_empty());
            row["managed"] = json!(if is_managed { "Yes" } else { "No" });
            row["managedPolicy"] = policy;
            row
        })
        .collect();
    if sort_field == "managed" {
        page.sort_by(|a, b| match sort_direction.as_str() {
            "asc" => text(a, "managed").cmp(&text(b, "managed")),
            _ => text(b, "managed").cmp(&text(a, "managed")),
        });
    }
    ok(json!({"indices": page, "totalIndices": total}))
}

/// The data streams, by name.
pub fn data_streams(engine: &Engine, search: Option<&str>) -> Value {
    let pattern = match search {
        Some(s) if !s.is_empty() => format!("*{s}*"),
        _ => "*".to_string(),
    };
    match call(engine, "GET", &format!("/_data_stream/{}", encoded(&pattern)), None) {
        Ok(found) => {
            let streams = found.get("data_streams").cloned().unwrap_or_else(|| json!([]));
            let total = streams.as_array().map(|a| a.len()).unwrap_or(0);
            ok(json!({"dataStreams": streams, "totalDataStreams": total}))
        }
        Err(e) => json!({"ok": false, "error": e.message}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_path_did_not_take_goes_on_the_query() {
        let params: Map<String, Value> = serde_json::from_value(json!({
            "index": "a,b", "format": "json", "h": ["index", "health"], "body": {"x": 1}
        }))
        .unwrap();
        assert_eq!(query_of(&params, &["index"]), "?format=json&h=index%2Chealth");
        assert_eq!(segment(params.get("h")), Some("index,health".to_string()));
        assert_eq!(encoded("logs-*,.ds*"), "logs-*,.ds*");
    }
}
