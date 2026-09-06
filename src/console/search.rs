//! The searches the pages run.
//!
//! A visualization does not talk to the engine: it asks the console, which
//! adds what the operator's settings say -- a shard timeout, whether to wait
//! for an exact hit count -- and carries the answer back with one change,
//! `hits.total` as a number rather than the engine's `{value, relation}`,
//! because the front end was written for the number. Three routes: the
//! multi-search Discover and the dashboards use, the single search of the
//! `opensearch` strategy, and the value suggestions the filter editor
//! offers as somebody types.

use serde_json::{Map, Value, json};

use super::engine::{Engine, Failed};
use super::saved::{Looking, Saved};

/// What the server being replaced adds to every search from its own
/// configuration: `opensearch.shardTimeout`, thirty seconds by default.
const SHARD_TIMEOUT: &str = "30000ms";

/// `hits.total` as the front end reads it: a number.
pub fn shim_hits_total(mut response: Value) -> Value {
    if let Some(hits) = response.get_mut("hits").and_then(|v| v.as_object_mut())
        && let Some(value) = hits.get("total").and_then(|t| t.get("value")).cloned()
    {
        hits.insert("total".into(), value);
    }
    response
}

/// The engine's refusal, in the shape the front end reads: the message,
/// and the engine's own error under `attributes.error`.
fn refused(status: u16, body: &Value) -> Failed {
    let message = body
        .pointer("/error/reason")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| body.to_string());
    Failed { status, message, objects: None, error: None, attributes: None }
        .with_error(body.get("error").cloned().unwrap_or(Value::Null))
}

/// Several searches in one request, each answered in its place.
///
/// The body is `{searches: [{header: {index, preference?}, body}]}`; each
/// header goes to the engine with `ignore_unavailable` set and each body
/// with the shard timeout, so a dashboard over an index that is not there
/// draws its other panels.
pub fn msearch(engine: &Engine, body: &Value) -> Result<Value, Failed> {
    let searches = body.get("searches").and_then(|v| v.as_array()).ok_or_else(|| {
        Failed::of(
            400,
            "[request body.searches]: expected value of type [array] but got [undefined]",
        )
    })?;
    let mut lines = String::new();
    for (n, search) in searches.iter().enumerate() {
        let header = search.get("header").and_then(|v| v.as_object()).ok_or_else(|| {
            Failed::of(
                400,
                format!("[request body.searches.{n}.header]: expected value of type [object] but got [undefined]"),
            )
        })?;
        if !header.get("index").is_some_and(|v| v.is_string()) {
            return Err(Failed::of(
                400,
                format!(
                    "[request body.searches.{n}.header.index]: expected value of type [string] but got [undefined]"
                ),
            ));
        }
        if let Some(preference) = header.get("preference")
            && !preference.is_string()
            && !preference.is_number()
        {
            return Err(Failed::of(
                400,
                format!(
                    "[request body.searches.{n}.header.preference]: expected value of type [number] or [string]"
                ),
            ));
        }
        let mut full_header = Map::new();
        full_header.insert("ignore_unavailable".into(), json!(true));
        for (k, v) in header {
            full_header.insert(k.clone(), v.clone());
        }
        let mut full_body = Map::new();
        full_body.insert("timeout".into(), json!(SHARD_TIMEOUT));
        if let Some(given) = search.get("body").and_then(|v| v.as_object()) {
            for (k, v) in given {
                full_body.insert(k.clone(), v.clone());
            }
        }
        lines.push_str(&Value::Object(full_header).to_string());
        lines.push('\n');
        lines.push_str(&Value::Object(full_body).to_string());
        lines.push('\n');
    }
    let answer = engine.raw(
        "POST",
        "/_msearch?ignore_unavailable=true",
        lines.as_bytes(),
        "application/x-ndjson",
    )?;
    let found: Value = serde_json::from_slice(&answer.body)
        .map_err(|e| Failed::of(502, format!("the engine's answer could not be read: {e}")))?;
    if answer.status >= 300 {
        return Err(refused(answer.status, &found));
    }
    let responses: Vec<Value> = found
        .get("responses")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(shim_hits_total)
        .collect();
    // the shape the client library hands back: the engine's answer under
    // `body`, beside the status and the headers
    Ok(json!({
        "body": {"responses": responses},
        "statusCode": answer.status,
        "headers": {"content-type": answer.content_type},
        "warnings": answer.warning,
        "meta": {},
    }))
}

/// One search, by the `opensearch` strategy: the request's `params` are
/// the engine's search parameters, `index` and `body` among them, and the
/// rest go on the query string.
pub fn search(engine: &Engine, request: &Value) -> Result<Value, Failed> {
    if request.get("indexType").is_some_and(|v| !v.is_null()) {
        return Err(Failed::of(
            500,
            format!(
                "Unsupported index pattern type {}",
                request.get("indexType").and_then(|v| v.as_str()).unwrap_or("")
            ),
        ));
    }
    let params = request.get("params").and_then(|v| v.as_object()).cloned().unwrap_or_default();
    let index = params
        .get("index")
        .map(|v| match v {
            Value::Array(items) => {
                items.iter().filter_map(|i| i.as_str()).collect::<Vec<_>>().join(",")
            }
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    let body = params.get("body").cloned().unwrap_or_else(|| json!({}));
    // the defaults the server being replaced sends, then the request's own
    let mut query: Vec<(String, String)> = vec![
        ("ignore_unavailable".into(), "true".into()),
        ("track_total_hits".into(), "true".into()),
        ("timeout".into(), SHARD_TIMEOUT.into()),
    ];
    for (key, value) in &params {
        if key == "index" || key == "body" {
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
        let key = snake_case(key);
        query.retain(|(k, _)| *k != key);
        query.push((key, text));
    }
    let query: String = query
        .iter()
        .map(|(k, v)| {
            format!("{k}={}", form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>())
        })
        .collect::<Vec<_>>()
        .join("&");
    let path = match index.is_empty() {
        true => format!("/_search?{query}"),
        false => format!(
            "/{}/_search?{query}",
            percent_encoding::utf8_percent_encode(&index, percent_encoding::NON_ALPHANUMERIC)
                .to_string()
                .replace("%2C", ",")
                .replace("%2A", "*")
        ),
    };
    let answer = engine.raw("POST", &path, body.to_string().as_bytes(), "application/json")?;
    let found: Value = serde_json::from_slice(&answer.body)
        .map_err(|e| Failed::of(502, format!("the engine's answer could not be read: {e}")))?;
    if answer.status >= 300 {
        return Err(refused(answer.status, &found));
    }
    let shards = found.get("_shards").cloned().unwrap_or_default();
    let number = |key: &str| shards.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    Ok(json!({
        "isPartial": false,
        "isRunning": false,
        "rawResponse": shim_hits_total(found),
        "total": number("total"),
        "loaded": number("failed") + number("successful"),
    }))
}

/// `maxConcurrentShardRequests` as the engine spells it.
fn snake_case(key: &str) -> String {
    let mut out = String::new();
    for ch in key.chars() {
        if ch.is_ascii_uppercase() {
            out.push('_');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// The values a field holds that begin with what somebody has typed.
///
/// A terms aggregation over the field with an `include` for the prefix,
/// through the nested path where the field has one -- which is why the
/// index pattern is looked up: its field list says which fields are
/// nested. `boolFilter` narrows it to what the page is already showing.
pub fn suggestions(
    engine: &Engine,
    saved: &Saved<'_>,
    index: &str,
    field: &str,
    query: &str,
    bool_filter: Option<&Value>,
) -> Result<Value, Failed> {
    let nested_path = index_pattern_field(saved, index, field)
        .and_then(|f| f.pointer("/subType/nested/path").and_then(|v| v.as_str()).map(String::from));
    let escaped: String = query
        .chars()
        .map(|c| match c {
            '.' | '?' | '+' | '*' | '|' | '{' | '}' | '[' | ']' | '(' | ')' | '"' | '\\' | '#'
            | '@' | '&' | '<' | '>' | '~' => format!("\\{c}"),
            other => other.to_string(),
        })
        .collect();
    let terms = json!({"suggestions": {"terms": {
        "field": field,
        "include": format!("{escaped}.*"),
        "execution_hint": "map",
        "shard_size": 10,
    }}});
    let aggs = match &nested_path {
        Some(path) => json!({"nestedSuggestions": {"nested": {"path": path}, "aggs": terms}}),
        None => terms,
    };
    let body = json!({
        "size": 0,
        // the server being replaced's own defaults for this route
        "timeout": "1000ms",
        "terminate_after": 100000,
        "query": {"bool": {"filter": bool_filter.cloned().unwrap_or_else(|| json!([]))}},
        "aggs": aggs,
    });
    let path = format!(
        "/{}/_search",
        percent_encoding::utf8_percent_encode(index, percent_encoding::NON_ALPHANUMERIC)
            .to_string()
            .replace("%2C", ",")
            .replace("%2A", "*")
    );
    let found = engine.call("POST", &path, Some(&body)).map_err(|e| Failed::of(500, e.message))?;
    if let Some(error) = found.get("error") {
        return Err(Failed::of(500, error.to_string()));
    }
    let buckets = found
        .pointer("/aggregations/suggestions/buckets")
        .or_else(|| found.pointer("/aggregations/nestedSuggestions/suggestions/buckets"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(Value::Array(buckets.into_iter().filter_map(|b| b.get("key").cloned()).collect()))
}

/// The field as the index pattern whose title is `index` describes it.
fn index_pattern_field(saved: &Saved<'_>, index: &str, field: &str) -> Option<Value> {
    let looking = Looking {
        types: vec!["index-pattern".to_string()],
        fields: vec!["fields".to_string()],
        search: Some(format!("\"{index}\"")),
        search_fields: vec!["title".to_string()],
        ..Default::default()
    };
    let found = saved.find(&looking).ok()?;
    let first = found.get("saved_objects")?.as_array()?.first()?;
    let fields: Value =
        serde_json::from_str(first.pointer("/attributes/fields")?.as_str()?).ok()?;
    fields
        .as_array()?
        .iter()
        .find(|f| f.get("name").and_then(|v| v.as_str()) == Some(field))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hits_total_becomes_a_number() {
        let shimmed =
            shim_hits_total(json!({"hits": {"total": {"value": 3, "relation": "eq"}, "hits": []}}));
        assert_eq!(shimmed["hits"]["total"], 3);
        let already = shim_hits_total(json!({"hits": {"total": 3}}));
        assert_eq!(already["hits"]["total"], 3);
    }

    #[test]
    fn keys_go_to_the_engine_in_its_own_spelling() {
        assert_eq!(snake_case("maxConcurrentShardRequests"), "max_concurrent_shard_requests");
        assert_eq!(snake_case("preference"), "preference");
    }
}
