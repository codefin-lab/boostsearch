//! Search requests written as templates.
//!
//! A search template is a search body with holes in it: `{{field}}` stands
//! for what the request passes under that name. OpenSearch renders them with
//! Mustache, and this is the part of Mustache a search template uses -- a
//! value, a section that repeats or is skipped, a value written without
//! escaping, and the three functions it adds: `toJson`, `join` and `url`.

use super::*;

/// The template, with what the parameters say put in place of its holes.
pub fn render(template: &str, params: &Value) -> String {
    render_within(template, std::slice::from_ref(params))
}

/// The hole a template opens and never closes, if there is one.
pub fn unclosed(template: &str) -> Option<String> {
    let mut rest = template;
    while let Some(at) = rest.find("{{") {
        let after = &rest[at + 2..];
        // `{{{name}}` opens three braces and closes two: the hole is not
        // closed, whatever follows it
        if let Some(inner) = after.strip_prefix('{') {
            let name: String = inner
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
                .collect();
            let closing = format!("{name}}}}}}}");
            if !inner.starts_with(&closing) {
                return Some(name);
            }
        }
        match after.find("}}") {
            Some(end) => rest = &after[end + 2..],
            None => {
                let name: String = after
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
                    .collect();
                return Some(name);
            }
        }
    }
    None
}

/// The same, with the values a section put in front of the outer ones.
fn render_within(template: &str, scope: &[Value]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(at) = rest.find("{{") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[at..]);
            return out;
        };
        let tag = after[..end].trim().to_string();
        let mut tail = &after[end + 2..];
        match tag.chars().next() {
            // `{{#name}}...{{/name}}` repeats for a list, runs once for a
            // value that is there, and is skipped for one that is not
            Some('#') => {
                let name = tag[1..].trim().to_string();
                let (body, after_section) = section(tail, &name);
                out.push_str(&filled(&name, body, scope));
                tail = after_section;
            }
            // `{{^name}}...{{/name}}` is the other way about
            Some('^') => {
                let name = tag[1..].trim().to_string();
                let (body, after_section) = section(tail, &name);
                if !truthy(&look_up(&name, scope)) {
                    out.push_str(&render_within(body, scope));
                }
                tail = after_section;
            }
            Some('/') => {}
            Some('!') => {}
            // `{{{name}}}` is the value as it stands, without escaping
            Some('{') => {
                let name = tag[1..].trim().trim_end_matches('}').to_string();
                out.push_str(&as_text(&look_up(&name, scope)));
                tail = tail.strip_prefix('}').unwrap_or(tail);
            }
            Some('&') => {
                let name = tag[1..].trim().to_string();
                out.push_str(&as_text(&look_up(&name, scope)));
            }
            _ => out.push_str(&escaped(&as_text(&look_up(&tag, scope)))),
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// What a section holds, and what follows it.
fn section<'a>(text: &'a str, name: &str) -> (&'a str, &'a str) {
    let closing = format!("{{{{/{name}}}}}");
    match text.find(&closing) {
        Some(at) => (&text[..at], &text[at + closing.len()..]),
        None => (text, ""),
    }
}

/// Whether a template can be rendered at all, without rendering it.
///
/// A pipeline holding a template that cannot be rendered is refused when it
/// is written rather than at the first document, so the mistake is reported
/// to whoever made it. The one thing that can be wrong without any values to
/// render against is a function section: `{{#join}}{{/join}}` names no field
/// to join, and `{{#join}}a b{{/join}}` names two.
pub fn check(template: &str) -> Result<(), String> {
    for name in ["join", "toJson", "url"] {
        let opening = format!("{{{{#{name}}}}}");
        let closing = format!("{{{{/{name}}}}}");
        let mut rest = template;
        while let Some(at) = rest.find(&opening) {
            let after = &rest[at + opening.len()..];
            let Some(end) = after.find(&closing) else { break };
            if name != "url" && after[..end].split_whitespace().count() != 1 {
                return Err(format!(
                    "Mustache function [{name}] must contain one and only one identifier"
                ));
            }
            rest = &after[end + closing.len()..];
        }
    }
    Ok(())
}

/// A section, with what stands in it.
fn filled(name: &str, body: &str, scope: &[Value]) -> String {
    // the three functions a search template may name
    match name {
        "toJson" => {
            let inner = render_within(body, scope).trim().to_string();
            return look_up(&inner, scope).to_string();
        }
        "join" => {
            let inner = render_within(body, scope).trim().to_string();
            return match look_up(&inner, scope) {
                Value::Array(items) => items.iter().map(as_text).collect::<Vec<_>>().join(","),
                other => as_text(&other),
            };
        }
        "url" => {
            let inner = render_within(body, scope);
            return url_escaped(&inner);
        }
        _ => {}
    }
    match look_up(name, scope) {
        Value::Array(items) => items
            .iter()
            .map(|item| {
                let mut inner: Vec<Value> = vec![item.clone()];
                inner.extend_from_slice(scope);
                render_within(body, &inner)
            })
            .collect(),
        Value::Object(map) => {
            let mut inner: Vec<Value> = vec![Value::Object(map)];
            inner.extend_from_slice(scope);
            render_within(body, &inner)
        }
        other if truthy(&other) => render_within(body, scope),
        _ => String::new(),
    }
}

/// The value a name stands for, looked for in each scope from the inside out.
fn look_up(name: &str, scope: &[Value]) -> Value {
    if name == "." {
        return scope.first().cloned().unwrap_or(Value::Null);
    }
    for level in scope {
        let mut here = level;
        let mut found = true;
        for part in name.split('.') {
            match here.get(part) {
                Some(next) => here = next,
                None => {
                    found = false;
                    break;
                }
            }
        }
        if found {
            return here.clone();
        }
    }
    Value::Null
}

/// Whether a value makes a section run.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Array(a) => !a.is_empty(),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// A value as it is written into the template.
fn as_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The characters JSON cannot hold as they are.
fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out
}

/// The characters a URL cannot hold as they are.
fn url_escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The template a request names, whether written into it or stored.
fn template_of(store: &Store, body: &Value, id: Option<&str>) -> Option<Value> {
    if let Some(source) = body.get("source") {
        return Some(source.clone());
    }
    let named = id
        .map(|i| i.to_string())
        .or_else(|| body.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))?;
    store.stored_script(&named).and_then(|script| script.get("source").cloned())
}

/// A rendered template, read back as the search body it stands for.
/// The same text with whatever it left open closed again.
fn closed_up(text: &str) -> Option<Value> {
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for c in text.chars() {
        match (in_string, escaped, c) {
            (true, true, _) => escaped = false,
            (true, false, '\\') => escaped = true,
            (true, false, '"') => in_string = false,
            (true, false, _) => {}
            (false, _, '"') => in_string = true,
            (false, _, '{') => stack.push('}'),
            (false, _, '[') => stack.push(']'),
            (false, _, '}' | ']') => {
                stack.pop();
            }
            _ => {}
        }
    }
    let mut closed = text.to_string();
    while let Some(c) = stack.pop() {
        closed.push(c);
    }
    serde_json::from_str(&closed).ok()
}

/// A template filled in with the parameters it names, where it can be.
///
/// Used where a template stands for a query rather than for a whole request,
/// as `_rank_eval` writes them.
pub(crate) fn render_query_template(template: &Value, params: &Value) -> Option<Value> {
    rendered(template, params).ok()
}

fn rendered(template: &Value, params: &Value) -> std::result::Result<Value, String> {
    // a template may be written as a string or as the body itself
    let text = match template {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if let Some(name) = unclosed(&text) {
        return Err(format!("Improperly closed variable: {name} in query-template"));
    }
    let filled = render(&text, params);
    let read: std::result::Result<Value, _> = serde_json::from_str(&filled);
    let Err(e) = read else { return read.map_err(|e| e.to_string()) };
    if !e.is_eof() {
        return Err(format!("Failed to parse content to map: {e}"));
    }
    // a template that stops early is still read as far as it goes: a clause it
    // names that does not exist is a complaint about that clause, not about
    // where the text ran out
    if let Some(closed) = closed_up(&filled)
        && let Some(named) =
            closed.pointer("/query").and_then(|q| q.as_object()).and_then(|o| o.keys().next())
        && crate::query::unknown_clause(named)
    {
        return Err(format!("parsing_exception|unknown query [{named}]"));
    }
    Err(format!("Failed to parse content to map: {e}"))
}

pub async fn render_template(
    State(store): State<Store>,
    id: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let named = id.map(|Path(i)| i);
    let Some(template) = template_of(&store, &body, named.as_deref()) else {
        return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", "no template named");
    };
    let params = body.get("params").cloned().unwrap_or_else(|| json!({}));
    match rendered(&template, &params) {
        Ok(out) => respond(&p, json!({ "template_output": out })),
        Err(e) => err(StatusCode::BAD_REQUEST, "json_parse_exception", e),
    }
}

pub async fn search_template(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let Some(template) = template_of(&store, &body, None) else {
        // a template named by an id that nobody stored is a missing thing,
        // not a malformed request
        if let Some(id) = body.get("id").and_then(|v| v.as_str()) {
            return err(
                StatusCode::NOT_FOUND,
                "resource_not_found_exception",
                format!("unable to find script [{id}] in cluster state"),
            );
        }
        return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", "no template named");
    };
    let params = body.get("params").cloned().unwrap_or_else(|| json!({}));
    let request = match rendered(&template, &params) {
        Ok(out) => out,
        Err(e) => return err(StatusCode::BAD_REQUEST, "json_parse_exception", e),
    };
    // `explain` and `profile` are asked for beside the template, not inside it
    let mut request = request;
    for named in ["explain", "profile"] {
        if let Some(asked) = body.get(named) {
            request[named] = asked.clone();
        }
    }
    // the rendered body is a search body, and is answered as one
    match crate::search::run(&store, &expr, &request, &p) {
        Ok(out) => respond(&p, crate::search::envelope(out, &request, &p)),
        Err(r) => r,
    }
}

pub async fn put_script(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let Some(script) = body.get("script").cloned() else {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "Validation Failed: 1: must specify script;",
        );
    };
    if script.get("source").is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "Validation Failed: 1: must specify source for stored script;",
        );
    }
    // a script stored for a context is compiled first, so that a fault is
    // reported now rather than at the first search
    let lang = script.get("lang").and_then(|v| v.as_str()).unwrap_or("painless");
    if lang == "painless" && p.get("context").is_some() {
        if let Err(e) = crate::painless::contexts::Compiled::of(&script, &|_| None) {
            return crate::api::compile_failure(e);
        }
    }
    store.remember_script(&id, script);
    respond(&p, json!({"acknowledged": true}))
}

/// `PUT _scripts/{id}/{context}` -- the context named on the path.
pub async fn put_script_in_context(
    State(store): State<Store>,
    Path((id, context)): Path<(String, String)>,
    Query(mut p): Query<Params>,
    body: String,
) -> Response {
    p.insert("context".into(), context);
    put_script(State(store), Path(id), Query(p), body).await
}

pub async fn get_script(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    match store.stored_script(&id) {
        Some(script) => respond(&p, json!({"_id": id, "found": true, "script": script})),
        None => {
            let body = json!({"_id": id, "found": false});
            (StatusCode::NOT_FOUND, axum::Json(body)).into_response()
        }
    }
}

pub async fn delete_script(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if !store.forget_script(&id) {
        return err(
            StatusCode::NOT_FOUND,
            "resource_not_found_exception",
            format!("stored script [{id}] does not exist and cannot be deleted"),
        );
    }
    respond(&p, json!({"acknowledged": true}))
}

/// `_msearch/template` -- several templated searches in one request.
pub async fn msearch_template(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let default_index = index.map(|Path(i)| i).unwrap_or_default();
    let mut responses = Vec::new();
    let mut lines = body.lines().filter(|l| !l.trim().is_empty());
    while let (Some(head), Some(request)) = (lines.next(), lines.next()) {
        let head: Value = parse_body(head).unwrap_or(json!({}));
        let request: Value = parse_body(request).unwrap_or(json!({}));
        let expr = head
            .get("index")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| default_index.clone());
        // a search in the list that names no template at all is not one
        // failure among several: the whole request was written wrongly
        let empty = request.get("source").map(|v| match v {
            Value::String(text) => text.trim().is_empty(),
            other => other.is_null(),
        });
        if empty == Some(true) && request.get("id").is_none() {
            return err(
                StatusCode::BAD_REQUEST,
                "action_request_validation_exception",
                "Validation Failed: 1: template is missing;",
            );
        }
        let Some(template) = template_of(&store, &request, None) else {
            // a template named by an id nobody stored is a missing thing
            let (status, kind, reason) = match request.get("id").and_then(|v| v.as_str()) {
                Some(id) => (
                    404,
                    "resource_not_found_exception",
                    format!("unable to find script [{id}] in cluster state"),
                ),
                None => (400, "illegal_argument_exception", "no template named".to_string()),
            };
            responses.push(json!({
                "error": {
                    "type": kind,
                    "reason": reason.clone(),
                    "root_cause": [{"type": kind, "reason": reason}],
                },
                "status": status,
            }));
            continue;
        };
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        let filled = match rendered(&template, &params) {
            Ok(out) => out,
            Err(e) => {
                // a template that stops in the middle of its JSON is a
                // different complaint from one that is written wrongly, and
                // one that names a clause nobody knows is a third
                let (kind, reason) = match e.split_once('|') {
                    Some((named, reason)) => (named.to_string(), reason.to_string()),
                    None if e.contains("EOF") || e.contains("end of input") => (
                        "unexpected_end_of_input_exception".to_string(),
                        "Unexpected end of input".to_string(),
                    ),
                    None => ("json_parse_exception".to_string(), e),
                };
                responses.push(json!({
                    "error": {
                        "type": kind,
                        "reason": reason.clone(),
                        "root_cause": [{"type": kind, "reason": reason}],
                    },
                    "status": 400,
                }));
                continue;
            }
        };
        match crate::search::run(&store, &expr, &filled, &p) {
            Ok(out) => {
                // the request's own parameters -- how a total is written,
                // whether keys are typed -- hold for every answer in the list
                let mut env = crate::search::envelope(out, &filled, &p);
                env["status"] = json!(200);
                responses.push(env);
            }
            // the search's own complaint is what the caller is told, so that
            // an unknown query reads as an unknown query rather than as a
            // shard that failed
            Err(response) => responses.push(crate::api::as_error_body(response).await),
        }
    }
    respond(&p, json!({"took": 1, "responses": responses}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hole_is_filled_with_what_was_passed() {
        let params = json!({"value": "foo", "list": ["a", "b"], "on": true});
        assert_eq!(render("{{value}}", &params), "foo");
        assert_eq!(render("{{#on}}yes{{/on}}", &params), "yes");
        assert_eq!(render("{{^on}}no{{/on}}", &params), "");
        assert_eq!(render("{{#list}}[{{.}}]{{/list}}", &params), "[a][b]");
        assert_eq!(render("{{#join}}list{{/join}}", &params), "a,b");
        assert_eq!(render("{{#toJson}}list{{/toJson}}", &params), "[\"a\",\"b\"]");
        assert_eq!(render("{{#url}}a b{{/url}}", &params), "a%20b");
    }
}
