//! An index as a thing: made, opened, closed, resized, rolled over, deleted.

use super::*;

mod lifecycle;
pub use lifecycle::*;

mod resize;
pub use resize::*;
mod shards;
pub use shards::*;

pub async fn create_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(_p): Query<Params>,
    body: String,
) -> Response {
    // An endpoint this server does not answer falls through to here, and a
    // name beginning with an underscore is the API's own, never an index --
    // so a request to `/_reindex` is a request that went unanswered, not a
    // request to create an index called `_reindex`.
    if let Some(r) = reserved_index_name(&index) {
        return r;
    }
    if let Some(r) = bad_index_name(&index) {
        return r;
    }
    let body: Value = if body.trim().is_empty() {
        json!({})
    } else {
        match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => return err(StatusCode::BAD_REQUEST, "parse_exception", e.to_string()),
        }
    };
    if store.exists(&index) {
        return err(
            StatusCode::BAD_REQUEST,
            "resource_already_exists_exception",
            format!("index [{index}] already exists"),
        );
    }
    // a flat_object holds whatever it is given and is not analysed, so the
    // parameters that describe analysis mean nothing to it
    if let Some(props) = body.pointer("/mappings/properties").and_then(|p| p.as_object()) {
        for (name, def) in props {
            if def.get("type").and_then(|t| t.as_str()) != Some("flat_object") {
                continue;
            }
            let stray: Vec<String> = def
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter(|(k, _)| *k != "type")
                        .map(|(k, v)| {
                            format!(
                                "{k} : {}",
                                v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string())
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !stray.is_empty() {
                return err(
                    StatusCode::BAD_REQUEST,
                    "mapper_parsing_exception",
                    format!(
                        "Mapping definition for [{name}] has unsupported parameters:  [{}]",
                        stray.join(", ")
                    ),
                );
            }
        }
    }
    // an index can only sort itself by a field whose values it can compare,
    // one at a time
    if let Some(fields) = body
        .pointer("/settings/index.sort.field")
        .or_else(|| body.pointer("/settings/index/sort/field"))
    {
        let names: Vec<String> = match fields {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
            _ => Vec::new(),
        };
        for name in names {
            let kind = body
                .pointer(&format!(
                    "/mappings/properties/{}/type",
                    name.replace('.', "/properties/")
                ))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            // a field inside a nested object belongs to the object, not to
            // the document, so the document cannot be sorted by it
            let mut inside_nested = false;
            let mut walked = String::new();
            for part in name.split('.').rev().skip(1).collect::<Vec<_>>().into_iter().rev() {
                walked =
                    if walked.is_empty() { part.to_string() } else { format!("{walked}.{part}") };
                if body
                    .pointer(&format!(
                        "/mappings/properties/{}/type",
                        walked.replace('.', "/properties/")
                    ))
                    .and_then(|t| t.as_str())
                    == Some("nested")
                {
                    inside_nested = true;
                }
            }
            if inside_nested {
                return err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!(
                        "index sorting on nested fields is not supported: found nested sort \
                         field [{name}] in [{index}]"
                    ),
                );
            }
            if matches!(kind, "half_float" | "nested" | "object" | "text" | "") {
                return err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!("docvalues not found for index sort field:[{name}]"),
                );
            }
        }
    }
    // adaptive shard selection only makes sense on an append-only index
    let setting = |k: &str| -> Option<String> {
        body.pointer(&format!("/settings/index/{k}"))
            .or_else(|| body.pointer(&format!("/settings/index.{k}")))
            .map(|v| v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string()))
    };
    if setting("bulk.adaptive_shard_selection.enabled").as_deref() == Some("true")
        && setting("append_only.enabled").as_deref() != Some("true")
    {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "index [{index}] is not append-only index, bulk adaptive shard selection is \
                 enabled, which is not supported. Please disable bulk adaptive shard selection \
                 or set index to append-only index."
            ),
        );
    }
    if body
        .get("mappings")
        .map(|m| {
            m.get("properties")
                .and_then(|p| p.as_object())
                .map(|o| o.keys().any(|k| k.trim().is_empty()))
                .unwrap_or(false)
        })
        .unwrap_or(false)
    {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "field name cannot be an empty string",
        );
    }
    match store.create(&index, &body) {
        Ok(()) => axum::Json(json!({
            "acknowledged": true, "shards_acknowledged": true, "index": index
        }))
        .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string()),
    }
}

pub async fn delete_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let lenient = ignore_unavailable(&p);
    // an alias names indices without being one; deleting it would have to
    // mean deleting what it points at, which is not what was asked
    for part in index.split(',').map(|n| n.trim()).filter(|n| !n.is_empty()) {
        if !part.contains('*') && store.is_alias(part) && !lenient {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!(
                    "The provided expression [{part}] matches an alias, specify the \
                     corresponding concrete indices instead."
                ),
            );
        }
    }
    // a pattern reaches for indices, not for the aliases that stand in front
    // of them, so it is matched against the real names only
    let mut targets: Vec<String> = Vec::new();
    let mut missing: Option<String> = None;
    for part in index.split(',').map(|n| n.trim()).filter(|n| !n.is_empty()) {
        if part.contains('*') {
            // the cluster's indices, not this node's share of them: a pattern
            // that stopped at the local store would leave the rest standing
            for n in crate::api::cluster_names(&store) {
                if !crate::store::glob_match(part, &n) || targets.contains(&n) {
                    continue;
                }
                if store.get(&n).is_some() {
                    targets.push(n);
                } else if let Some(uuid) =
                    crate::cluster::with_state(|s| s.indices.get(&n).map(|m| m.uuid.clone()))
                {
                    // held elsewhere: the tombstone the manager publishes is
                    // what deletes it
                    store.tombstone(&n, &uuid);
                    crate::security::audit_index_event(&n, "indices:admin/delete", "{}", false);
                }
            }
        } else if store.is_alias(part) {
            continue;
        } else {
            let found = store.resolve(part);
            if found.is_empty() {
                // an index the cluster has that this node holds no copy of:
                // deleted by its tombstone, which the manager publishes
                let published =
                    crate::cluster::with_state(|s| s.indices.get(part).map(|m| m.uuid.clone()));
                match published {
                    Some(uuid) => {
                        store.tombstone(part, &uuid);
                        crate::security::audit_index_event(
                            part,
                            "indices:admin/delete",
                            "{}",
                            false,
                        );
                    }
                    None => {
                        missing.get_or_insert_with(|| part.to_string());
                    }
                }
            }
            for n in found {
                if !targets.contains(&n) {
                    targets.push(n);
                }
            }
        }
    }
    if let Some(name) = missing.filter(|_| !lenient) {
        return no_such_index(&name);
    }
    // `allow_no_indices=false` makes an expression that reaches nothing an
    // error rather than a request with nothing to do
    // `allow_no_indices=false` is asked of each pattern in turn: an
    // expression is only satisfied if every part of it reached something
    let allow_none = p.get("allow_no_indices").map(|v| v != "false").unwrap_or(true);
    if !allow_none {
        for part in index.split(',').map(|n| n.trim()).filter(|n| n.contains('*')) {
            let reached = store.names().iter().any(|n| crate::store::glob_match(part, n));
            if !reached {
                return no_such_index(part);
            }
        }
        if targets.is_empty() {
            return no_such_index(&index);
        }
    }
    for n in &targets {
        store.delete(n);
    }
    axum::Json(json!({"acknowledged": true})).into_response()
}

pub async fn index_exists(State(store): State<Store>, Path(index): Path<String>) -> Response {
    if store.resolve(&index).is_empty() {
        StatusCode::NOT_FOUND.into_response()
    } else {
        StatusCode::OK.into_response()
    }
}

/// The characters an index name may not carry, and the order OpenSearch
/// lists them in when it complains.
const FORBIDDEN_IN_NAME: &[char] = &[' ', '"', '*', '\\', '<', '|', ',', '>', '/', '?'];

/// A name a new index may not be given: OpenSearch names the whole set in
/// the complaint, whichever of them the name carried.
pub(crate) fn bad_index_name(name: &str) -> Option<Response> {
    if !name.contains(|c| FORBIDDEN_IN_NAME.contains(&c)) {
        return None;
    }
    Some(err(
        StatusCode::BAD_REQUEST,
        "invalid_index_name_exception",
        format!(
            "Invalid index name [{name}], must not contain the following characters \
             [ , \", *, \\, <, |, ,, >, /, ?]"
        ),
    ))
}

/// Names beginning with an underscore are reserved for the API's own
/// endpoints, so one cannot also be an index.
pub(crate) fn reserved_index_name(expr: &str) -> Option<Response> {
    for part in expr.split(',').map(|n| n.trim()) {
        if part.starts_with('_') && !matches!(part, "_all" | "_any") {
            return Some(err(
                StatusCode::BAD_REQUEST,
                "invalid_index_name_exception",
                format!("Invalid index name [{part}], must not start with '_'."),
            ));
        }
    }
    None
}

pub async fn get_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if let Some(r) = reserved_index_name(&index) {
        return r;
    }
    // `expand_wildcards` says which states a pattern reaches
    // health looks at every index by default, closed ones included: a closed
    // index still has shards, and they still count
    let states = p.get("expand_wildcards").map(|v| v.as_str()).unwrap_or("all");
    let want_open = states.split(',').any(|w| matches!(w.trim(), "open" | "all"));
    let want_closed = states.split(',').any(|w| matches!(w.trim(), "closed" | "all"));
    let targets = store.resolve(&index);
    if targets.is_empty() && !index.contains('*') && index != "_all" && !ignore_unavailable(&p) {
        return no_such_index(&index);
    }
    // a pattern reaching nothing is an error only when the caller said so
    let allow_none = p.get("allow_no_indices").map(|v| v != "false").unwrap_or(true);
    if targets.is_empty() && !allow_none {
        return no_such_index(&index);
    }
    let mut out = serde_json::Map::new();
    for n in targets {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        // a pattern only reaches the states it was told to; a name given
        // outright reaches its index whatever state it is in
        if index.contains('*') && ((g.closed && !want_closed) || (!g.closed && !want_open)) {
            continue;
        }
        // a pattern only reaches the states it was told to; a name given
        // outright reaches its index whatever state it is in
        if index.contains('*') && ((g.closed && !want_closed) || (!g.closed && !want_open)) {
            continue;
        }
        let mut aliases = serde_json::Map::new();
        for (a, def) in &g.aliases {
            aliases.insert(a.clone(), def.clone());
        }
        let mut settings = g.effective_settings();
        if flag(&p, "human") {
            add_human_settings(&mut settings, &g);
        }
        out.insert(
            n.clone(),
            json!({
                "aliases": Value::Object(aliases),
                "mappings": if g.mapping.raw.is_null() { json!({}) } else { g.mapping.raw.clone() },
                "settings": settings,
            }),
        );
    }
    respond(&p, Value::Object(out))
}
