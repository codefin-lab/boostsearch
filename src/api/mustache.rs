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
    serde_json::from_str(&filled).map_err(|e| format!("Failed to parse content to map: {e}"))
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
    store.remember_script(&id, script);
    respond(&p, json!({"acknowledged": true}))
}

pub async fn get_script(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    match store.stored_script(&id) {
        Some(script) => respond(&p, json!({"_id": id, "found": true, "script": script})),
        None => respond(&p, json!({"_id": id, "found": false})),
    }
}

pub async fn delete_script(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    store.forget_script(&id);
    respond(&p, json!({"acknowledged": true}))
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
        let Some(template) = template_of(&store, &request, None) else {
            responses.push(json!({
                "error": {
                    "type": "illegal_argument_exception",
                    "reason": "no template named",
                },
                "status": 400,
            }));
            continue;
        };
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        let filled = match rendered(&template, &params) {
            Ok(out) => out,
            Err(e) => {
                responses.push(json!({
                    "error": {"type": "json_parse_exception", "reason": e},
                    "status": 400,
                }));
                continue;
            }
        };
        match crate::search::run(&store, &expr, &filled, &Params::new()) {
            Ok(out) => {
                let mut env = crate::search::envelope(out, &filled, &Params::new());
                env["status"] = json!(200);
                responses.push(env);
            }
            Err(_) => responses.push(json!({
                "error": {
                    "type": "search_phase_execution_exception",
                    "reason": "all shards failed",
                },
                "status": 400,
            })),
        }
    }
    respond(&p, json!({"took": 1, "responses": responses}))
}
