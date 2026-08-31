//! What an index says its fields are.

use super::*;

pub(crate) fn mapping_view(st: &IdxState) -> Value {
    let mut m = if st.mapping.raw.is_null() { json!({}) } else { st.mapping.raw.clone() };
    add_type_defaults(&mut m);
    json!({"mappings": m})
}

pub async fn get_mapping(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_else(|| "_all".into());
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !ignore_unavailable(&p) {
        return no_such_index(&expr);
    }
    // `expand_wildcards` says which states a pattern reaches; `none` means it
    // reaches nothing at all
    // health looks at every index by default, closed ones included: a closed
    // index still has shards, and they still count
    let states = p.get("expand_wildcards").map(|v| v.as_str()).unwrap_or("all");
    let reach = |closed: bool| {
        states.split(',').any(|w| match w.trim() {
            "all" => true,
            "open" => !closed,
            "closed" => closed,
            _ => false,
        })
    };
    let mut out = serde_json::Map::new();
    for n in targets {
        if let Some(st) = store.get(&n) {
            let g = st.read();
            if expr.contains('*') && !reach(g.closed) {
                continue;
            }
            out.insert(n, mapping_view(&g));
        }
    }
    // a pattern reaching nothing is an error only when the caller said so
    let allow_none = p.get("allow_no_indices").map(|v| v != "false").unwrap_or(true);
    if out.is_empty() && !allow_none {
        return no_such_index(&expr);
    }
    axum::Json(Value::Object(out)).into_response()
}

pub async fn put_mapping(
    State(store): State<Store>,
    Path(index): Path<String>,
    body: String,
) -> Response {
    let body: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    // a mapping names fields, not a type; the type that used to wrap them is
    // gone and a body still carrying one is not a mapping
    if body.get("_doc").is_some() && body.get("properties").is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "Types cannot be provided in put mapping requests",
        );
    }
    // a field has to be called something
    fn empty_named(node: &Value) -> bool {
        let Some(props) = node.get("properties").and_then(|p| p.as_object()) else {
            return false;
        };
        props.keys().any(|k| k.trim().is_empty()) || props.values().any(empty_named)
    }
    if empty_named(&body) {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "field name cannot be an empty string",
        );
    }
    let targets = store.resolve(&index);
    if targets.is_empty() {
        return no_such_index(&index);
    }
    for n in targets {
        if let Some(st) = store.get(&n) {
            let mut g = st.write();
            g.mapping.merge(&body);
        }
    }
    axum::Json(json!({"acknowledged": true})).into_response()
}

pub async fn get_field_mapping(
    State(store): State<Store>,
    path: Path<Vec<String>>,
    Query(p): Query<Params>,
) -> Response {
    let Path(parts) = path;
    let (expr, fields) = match parts.len() {
        1 => ("_all".to_string(), parts[0].clone()),
        _ => (parts[0].clone(), parts[1].clone()),
    };
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !ignore_unavailable(&p) {
        return no_such_index(&expr);
    }
    let wanted: Vec<&str> = fields.split(',').map(|s| s.trim()).collect();
    let mut out = serde_json::Map::new();
    for n in targets {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        let mut mappings = serde_json::Map::new();
        let declared = g.mapping.raw.get("properties").and_then(|v| v.as_object()).cloned();
        for (path_name, kind) in g.all_field_types() {
            let hit = wanted.iter().any(|w| {
                *w == "*"
                    || *w == path_name
                    || crate::store::wildcard_to_regex(w).is_match(&path_name)
                    || path_name.rsplit('.').next() == Some(*w)
            });
            if !hit {
                continue;
            }
            // echo the declared definition when there is one, else the type we learned
            let def = declared
                .as_ref()
                .and_then(|d| d.get(path_name.split('.').next().unwrap_or(&path_name)))
                .filter(|_| !path_name.contains('.'))
                .cloned()
                .unwrap_or_else(|| json!({"type": kind}));
            let leaf = path_name.rsplit('.').next().unwrap_or(&path_name).to_string();
            // `include_defaults` fills in what the field would use where it
            // did not say, which for a text field is the default analyzer
            let mut def = def;
            if flag(&p, "include_defaults") {
                if let Some(o) = def.as_object_mut() {
                    if o.get("type").and_then(|t| t.as_str()) == Some("text") {
                        o.entry("analyzer".to_string()).or_insert_with(|| json!("default"));
                    }
                }
            }
            mappings.insert(
                path_name.clone(),
                json!({
                    "full_name": path_name,
                    "mapping": { leaf: def }
                }),
            );
        }
        out.insert(n.clone(), json!({"mappings": Value::Object(mappings)}));
    }
    respond(&p, Value::Object(out))
}
