//! Templates: what an index gets before anyone asks for it.

use super::*;

pub async fn put_template(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let mut body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    // `template` is the older spelling of `index_patterns`
    if body.get("index_patterns").is_none() {
        if let Some(t) = body.get("template").cloned() {
            body["index_patterns"] = match t {
                Value::String(s) => json!([s]),
                other => other,
            };
        }
    }
    if body.get("index_patterns").is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: index patterns are missing;",
        );
    }
    // `create` says this is a new template, so one already there is not to be
    // written over
    if p.get("create").map(|v| v != "false").unwrap_or(false)
        && store.get_templates().contains_key(&name)
    {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("index_template [{name}] already exists"),
        );
    }
    // one pattern may be written without its brackets; a template always
    // reports a list, however few patterns it holds
    if let Some(Value::String(one)) = body.get("index_patterns").cloned() {
        body["index_patterns"] = json!([one]);
    }
    store.put_template(&name, body);
    respond(&p, json!({"acknowledged": true}))
}

pub async fn get_template(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let all = store.get_templates();
    let want = name.map(|Path(n)| n);
    let mut out = serde_json::Map::new();
    for (k, v) in all {
        let hit = match &want {
            None => true,
            Some(n) if n == "*" || n == "_all" => true,
            Some(n) => n.split(',').any(|pat| {
                pat == k || crate::store::wildcard_to_regex(pat.trim()).is_match(&k)
            }),
        };
        if hit {
            // a template reports its settings the way an index does: nested
            // under `index`, values as text
            let mut v = v;
            if let Some(o) = v.as_object_mut() {
                o.remove("__composable");
                o.entry("mappings".to_string()).or_insert_with(|| json!({}));
                o.entry("aliases".to_string()).or_insert_with(|| json!({}));
                if let Some(set) = o.get("settings").cloned() {
                    let nested = template_settings(&set);
                    let flat = p.get("flat_settings").map(|v| v == "true").unwrap_or(false);
                    o.insert("settings".into(), if flat {
                        let mut m = serde_json::Map::new();
                        flatten_cluster_settings(&nested, "", &mut m);
                        Value::Object(m)
                    } else {
                        nested
                    });
                }
                if let Some(Value::Object(aliases)) = o.get("aliases").cloned() {
                    let expanded: serde_json::Map<String, Value> = aliases
                        .into_iter()
                        .map(|(a, def)| (a, crate::store::normalize_alias(&def)))
                        .collect();
                    o.insert("aliases".into(), Value::Object(expanded));
                }
            }
            out.insert(k, v);
        }
    }
    if out.is_empty() {
        if let Some(n) = &want {
            if !n.contains('*') && n != "_all" {
                return err(
                    StatusCode::NOT_FOUND,
                    "resource_not_found_exception",
                    format!("index_template [{n}] missing"),
                );
            }
        }
    }
    respond(&p, Value::Object(out))
}

pub async fn exists_template(State(store): State<Store>, Path(name): Path<String>) -> Response {
    let all = store.get_templates();
    let hit = all.keys().any(|k| {
        name.split(',').any(|pat| pat == k || crate::store::wildcard_to_regex(pat.trim()).is_match(k))
    });
    if hit { StatusCode::OK.into_response() } else { StatusCode::NOT_FOUND.into_response() }
}

pub async fn delete_template(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if !store.delete_template(&name) {
        return err(
            StatusCode::NOT_FOUND,
            "index_template_missing_exception",
            format!("index_template [{name}] missing"),
        );
    }
    respond(&p, json!({"acknowledged": true}))
}

/// Settings as an index template carries them: nested under `index`, values
/// as text, whichever way they were written.
pub(crate) fn template_settings(v: &Value) -> Value {
    let mut flat = serde_json::Map::new();
    flatten_cluster_settings(v, "", &mut flat);
    let mut inner = serde_json::Map::new();
    for (k, val) in flat {
        inner.insert(k.strip_prefix("index.").unwrap_or(&k).to_string(), val);
    }
    // `index.blocks.write` names one setting three levels down, not a key
    // with dots in it
    json!({"index": nest_settings(&Value::Object(inner))})
}

/// `_component_template` -- settings and mappings named once, to be composed
/// into whichever index templates ask for them.
pub async fn put_component_template(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if body.get("template").is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: a template is required;",
        );
    }
    store.put_component(&name, body);
    respond(&p, json!({"acknowledged": true}))
}

pub async fn get_component_template(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let all = store.get_components();
    let wanted = name.map(|Path(n)| n);
    let mut out = Vec::new();
    for (n, body) in &all {
        let keep = match wanted.as_deref() {
            None | Some("*") | Some("_all") => true,
            Some(w) => w.split(',').any(|pat| {
                pat.trim() == n || crate::store::glob_match(pat.trim(), n)
            }),
        };
        if keep {
            let mut body = body.clone();
            if let Some(set) = body.pointer("/template/settings").cloned() {
                body["template"]["settings"] = template_settings(&set);
            }
            out.push(json!({"name": n, "component_template": body}));
        }
    }
    if out.is_empty() {
        if let Some(w) = wanted.as_deref().filter(|w| !w.contains('*')) {
            return err(
                StatusCode::NOT_FOUND,
                "resource_not_found_exception",
                format!("component template matching [{w}] not found"),
            );
        }
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    respond(&p, json!({"component_templates": out}))
}

pub async fn delete_component_template(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if !store.delete_component(&name) && !name.contains('*') {
        return err(
            StatusCode::NOT_FOUND,
            "resource_not_found_exception",
            format!("component template matching [{name}] not found"),
        );
    }
    respond(&p, json!({"acknowledged": true}))
}

/// Compose one index template body into the index it would create: the
/// components it names in the order it names them, then its own template,
/// each layer winning over the one before.
pub(crate) fn compose_template(store: &Store, body: &Value) -> Value {
    let components = store.get_components();
    let mut settings = json!({});
    let mut mappings = json!({});
    let mut aliases = json!({});
    let mut layers: Vec<Value> = Vec::new();
    if let Some(list) = body.get("composed_of").and_then(|v| v.as_array()) {
        for name in list.iter().filter_map(|v| v.as_str()) {
            if let Some(c) = components.get(name).and_then(|c| c.get("template")) {
                layers.push(c.clone());
            }
        }
    }
    if let Some(t) = body.get("template") {
        layers.push(t.clone());
    }
    for layer in layers {
        if let Some(v) = layer.get("settings") {
            crate::store::deep_merge(&mut settings, &template_settings(v));
        }
        if let Some(v) = layer.get("mappings") {
            crate::store::deep_merge(&mut mappings, v);
        }
        // an alias is defined whole: a later layer replaces the definition
        // rather than adding to it
        if let Some(Value::Object(o)) = layer.get("aliases") {
            let Some(slot) = aliases.as_object_mut() else { continue };
            for (name, def) in o {
                slot.insert(name.clone(), def.clone());
            }
        }
    }
    json!({"settings": settings, "mappings": mappings, "aliases": aliases})
}

/// Which other index templates claim any of the same patterns.
pub(crate) fn overlapping_templates(store: &Store, skip: &str, patterns: &[String]) -> Vec<Value> {
    let mut out = Vec::new();
    // both spellings of template claim patterns, so both can overlap
    for (name, t) in store.get_templates() {
        if name == skip {
            continue;
        }
        let pats: Vec<String> = t
            .get("index_patterns")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        if pats.iter().any(|a| patterns.iter().any(|b| patterns_overlap(a, b))) {
            out.push(json!({"name": name, "index_patterns": pats}));
        }
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    out
}

/// `_index_template/_simulate/{name}` -- what the named template, or one sent
/// in the body, would produce.
pub async fn simulate_template(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let sent: Value = parse_body(&body).unwrap_or(json!({}));
    let named = name.map(|Path(n)| n);
    // a body wins over the stored template of the same name, which is how a
    // replacement is tried out before it is saved
    let source = if sent.get("index_patterns").is_some() || sent.get("template").is_some() {
        sent.clone()
    } else {
        match named.as_deref().and_then(|n| store.get_templates().get(n).cloned()) {
            Some(t) => t.get("__composable").cloned().unwrap_or(t),
            None => {
                return err(
                    StatusCode::NOT_FOUND,
                    "resource_not_found_exception",
                    format!("index template matching [{}] not found", named.unwrap_or_default()),
                );
            }
        }
    };
    let patterns: Vec<String> = match source.get("index_patterns") {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(|s| s.into())).collect(),
        Some(Value::String(one)) => vec![one.clone()],
        _ => Vec::new(),
    };
    let skip = named.unwrap_or_default();
    respond(&p, json!({
        "template": compose_template(&store, &source),
        "overlapping": overlapping_templates(&store, &skip, &patterns),
    }))
}

/// `_index_template/_simulate_index/{index}` -- which template a name would
/// pick up, and what it would give it.
pub async fn simulate_index_template(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let sent: Value = parse_body(&body).unwrap_or(json!({}));
    let mut best: Option<(i64, String, Value)> = None;
    let mut matching: Vec<(String, Vec<String>)> = Vec::new();
    let mut consider = |name: String, t: &Value| {
        let pats: Vec<String> = match t.get("index_patterns") {
            Some(Value::Array(a)) => {
                a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()
            }
            Some(Value::String(one)) => vec![one.clone()],
            _ => Vec::new(),
        };
        if !pats.iter().any(|pat| crate::store::glob_match(pat, &index)) {
            return;
        }
        let prio = t
            .get("priority")
            .or_else(|| t.get("order"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        matching.push((name.clone(), pats));
        if best.as_ref().map(|(p, _, _)| prio > *p).unwrap_or(true) {
            best = Some((prio, name, t.clone()));
        }
    };
    // a template sent in the body is considered alongside the stored ones
    if sent.get("index_patterns").is_some() {
        consider(String::new(), &sent);
    }
    for (name, t) in store.get_templates() {
        let Some(body) = t.get("__composable") else { continue };
        consider(name, body);
    }
    // a name no template claims gets nothing, not an empty template
    let Some((_, winner, source)) = best else {
        return respond(&p, json!({}));
    };
    let mut overlapping: Vec<Value> = matching
        .into_iter()
        .filter(|(n, _)| *n != winner && !n.is_empty())
        .map(|(n, pats)| json!({"name": n, "index_patterns": pats}))
        .collect();
    // the legacy templates a name would also have picked up
    for (name, t) in store.get_templates() {
        if name == winner || t.get("__composable").is_some() {
            continue;
        }
        let pats: Vec<String> = t
            .get("index_patterns")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        if pats.iter().any(|pat| crate::store::glob_match(pat, &index)) {
            overlapping.push(json!({"name": name, "index_patterns": pats}));
        }
    }
    overlapping.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    respond(&p, json!({
        "template": compose_template(&store, &source),
        "overlapping": overlapping,
    }))
}

pub async fn put_index_template(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if body.get("index_patterns").is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: index patterns are missing;",
        );
    }
    // the composable form nests settings/mappings/aliases under `template`
    let patterns = match body["index_patterns"].clone() {
        Value::String(one) => json!([one]),
        other => other,
    };
    let mut flat = json!({"index_patterns": patterns});
    if let Some(order) = body.get("priority").or_else(|| body.get("order")) {
        flat["order"] = order.clone();
    }
    // what the template will actually make is its components in the order it
    // names them and then its own template, each layer winning over the last
    let composed = compose_template(&store, &body);
    for k in ["settings", "mappings", "aliases"] {
        if composed.get(k).map(|v| v.as_object().map(|o| !o.is_empty()).unwrap_or(false))
            == Some(true)
        {
            flat[k] = composed[k].clone();
        }
    }
    // the body is kept as the composable form's own answer, so it is stored
    // in the shape that answer has: patterns as a list, settings nested under
    // `index` with text values
    let mut kept = body.clone();
    kept["index_patterns"] = flat["index_patterns"].clone();
    if let Some(set) = kept.pointer("/template/settings").cloned() {
        kept["template"]["settings"] = template_settings(&set);
    }
    // an alias's routing stands for both halves of it
    if let Some(Value::Object(aliases)) = kept.pointer("/template/aliases").cloned() {
        let expanded: serde_json::Map<String, Value> = aliases
            .into_iter()
            .map(|(a, def)| (a, crate::store::normalize_alias(&def)))
            .collect();
        kept["template"]["aliases"] = Value::Object(expanded);
    }
    if p.get("create").map(|v| v != "false").unwrap_or(false)
        && store.get_templates().contains_key(&name)
    {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("index_template [{name}] already exists"),
        );
    }
    flat["__composable"] = kept;
    store.put_template(&name, flat);
    respond(&p, json!({"acknowledged": true}))
}

pub async fn get_index_template(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let want = name.map(|Path(n)| n);
    let mut list = Vec::new();
    for (k, v) in store.get_templates() {
        let hit = match &want {
            None => true,
            Some(n) if n == "*" || n == "_all" => true,
            Some(n) => n.split(',').any(|pat| {
                pat == k || crate::store::wildcard_to_regex(pat.trim()).is_match(&k)
            }),
        };
        if !hit {
            continue;
        }
        let body = v.get("__composable").cloned().unwrap_or_else(|| v.clone());
        list.push(json!({"name": k, "index_template": body}));
    }
    if list.is_empty() {
        if let Some(n) = &want {
            if !n.contains('*') && n != "_all" {
                return err(
                    StatusCode::NOT_FOUND,
                    "resource_not_found_exception",
                    format!("index template matching [{n}] not found"),
                );
            }
        }
    }
    respond(&p, json!({"index_templates": list}))
}

pub async fn delete_index_template(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    // a template a data stream was actually made from cannot be taken away;
    // one that merely claims the same patterns can
    let in_use: Vec<String> = store
        .data_streams()
        .into_iter()
        .filter(|(_, t)| *t == name)
        .map(|(n, _)| n)
        .collect();
    if !in_use.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "unable to remove composable templates [{name}] as they are in use by a data \
                 streams [{}]",
                in_use.join(",")
            ),
        );
    }
    if !store.delete_template(&name) {
        return err(
            StatusCode::NOT_FOUND,
            "resource_not_found_exception",
            format!("index template matching [{name}] not found"),
        );
    }
    respond(&p, json!({"acknowledged": true}))
}
