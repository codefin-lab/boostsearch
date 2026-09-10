//! Aliases: another name for an index, and the rules that come with it.

use super::*;

pub(crate) fn alias_view(
    store: &Store,
    index_expr: Option<&str>,
    name_expr: Option<&str>,
    states: Option<&str>,
) -> Value {
    // every index the cluster holds: an alias belongs to the index, and its
    // copies may be on any node
    let targets = match index_expr {
        Some(e) => crate::api::cluster_resolve(store, e),
        None => crate::api::cluster_names(store),
    };
    // `expand_wildcards` says which states to include; without it, both
    let (want_open, want_closed) = match states {
        None => (true, true),
        Some(v) => (
            v.split(',').any(|w| matches!(w.trim(), "open" | "all")),
            v.split(',').any(|w| matches!(w.trim(), "closed" | "all")),
        ),
    };
    let published = crate::cluster::current_state();
    let closed_of = |n: &String| -> Option<bool> {
        match store.get(n) {
            Some(st) => Some(st.read().closed),
            None => published.indices.get(n).map(|m| m.state == "close"),
        }
    };
    let targets: Vec<String> = targets
        .into_iter()
        .filter(|n| closed_of(n).map(|c| if c { want_closed } else { want_open }).unwrap_or(false))
        .collect();
    let mut out = serde_json::Map::new();
    for n in targets {
        let held: std::collections::BTreeMap<String, Value> = match store.get(&n) {
            Some(st) => st.read().aliases.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            None => published
                .indices
                .get(&n)
                .and_then(|m| m.aliases.as_object().cloned())
                .map(|o| o.into_iter().collect())
                .unwrap_or_default(),
        };
        let mut aliases = serde_json::Map::new();
        for (a, def) in &held {
            if alias_name_wanted(name_expr, a) {
                aliases.insert(a.clone(), def.clone());
            }
        }
        // naming an alias asks which indices carry it, so one that carries
        // none is not an answer; asking without a name asks about the indices
        // themselves, and an index with no aliases is still one of them
        if aliases.is_empty() && name_expr.is_some() {
            continue;
        }
        out.insert(n.clone(), json!({"aliases": Value::Object(aliases)}));
    }
    Value::Object(out)
}

/// Does an alias fall inside the expression naming it?
///
/// The expression is a comma list where a leading `-` removes rather than
/// adds, so `test_alias*,-test_alias_1` is every matching alias but that one.
pub(crate) fn alias_name_wanted(expr: Option<&str>, alias: &str) -> bool {
    let Some(expr) = expr.filter(|e| !e.is_empty()) else { return true };
    if matches!(expr, "*" | "_all") {
        return true;
    }
    let mut wanted = false;
    for pat in expr.split(',') {
        let pat = pat.trim();
        let (neg, pat) = match pat.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, pat),
        };
        let hit = pat == alias
            || pat == "*"
            || pat == "_all"
            || crate::store::wildcard_to_regex(pat).is_match(alias);
        if hit {
            wanted = !neg;
        }
    }
    wanted
}

/// The 404 this endpoint answers with carries the reason as a bare string
/// rather than the usual error object.
pub(crate) fn aliases_missing_response(names: &[String], view: &Value) -> Response {
    let mut names: Vec<String> = names.to_vec();
    names.sort();
    let label = if names.len() > 1 { "aliases" } else { "alias" };
    // the aliases that were found are still reported alongside the complaint
    let mut body = view.clone();
    if !body.is_object() {
        body = json!({});
    }
    if let Some(o) = body.as_object_mut() {
        o.insert("error".into(), json!(format!("{label} [{}] missing", names.join(","))));
        o.insert("status".into(), json!(404));
    }
    (StatusCode::NOT_FOUND, axum::Json(body)).into_response()
}

/// Which of the named aliases do not exist at all.
///
/// A name that exists but was removed from the answer by a later exclusion is
/// not missing -- it is excluded. Only a name nothing carries is missing.
/// Exclusions at the head of the list are a separate complaint: they have
/// nothing to exclude from, and are reported as written.
pub(crate) fn alias_names_missing(
    store: &Store,
    idx: Option<&str>,
    expr: Option<&str>,
) -> Vec<String> {
    let Some(expr) = expr.filter(|e| !e.is_empty()) else { return Vec::new() };
    // the run of plain exclusions at the head, ending at the first entry that
    // adds something or carries a wildcard
    let mut leading = Vec::new();
    for pat in expr.split(',').map(|p| p.trim()) {
        if !pat.starts_with('-') || pat.contains('*') {
            break;
        }
        leading.push(pat.to_string());
    }
    if !leading.is_empty() {
        return leading;
    }
    let targets = match idx {
        Some(e) => store.resolve(e),
        None => store.names(),
    };
    let existing: std::collections::HashSet<String> = targets
        .iter()
        .filter_map(|n| store.get(n))
        .flat_map(|st| st.read().aliases.keys().cloned().collect::<Vec<_>>())
        .collect();
    expr.split(',')
        .map(|p| p.trim())
        .filter(|p| !p.starts_with('-') && !p.contains('*') && *p != "_all" && !p.is_empty())
        .filter(|p| !existing.contains(*p))
        .map(|p| p.to_string())
        .collect()
}

/// `GET /{index}/_alias` -- every alias on the named indices.
pub async fn index_alias_list(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if store.resolve(&index).is_empty() {
        return no_such_index(&index);
    }
    respond(
        &p,
        alias_view(&store, Some(&index), None, p.get("expand_wildcards").map(|v| v.as_str())),
    )
}

pub async fn get_alias_scoped(
    State(store): State<Store>,
    path: Option<Path<Vec<String>>>,
    Query(p): Query<Params>,
) -> Response {
    let parts = path.map(|Path(v)| v).unwrap_or_default();
    let (idx, name) = match parts.len() {
        0 => (None, None),
        1 => (None, Some(parts[0].clone())),
        _ => (Some(parts[0].clone()), Some(parts[1].clone())),
    };
    let view = alias_view(
        &store,
        idx.as_deref(),
        name.as_deref(),
        p.get("expand_wildcards").map(|v| v.as_str()),
    );
    let missing = alias_names_missing(&store, idx.as_deref(), name.as_deref());
    if !missing.is_empty() {
        return aliases_missing_response(&missing, &view);
    }
    respond(&p, view)
}

pub async fn index_alias_get(
    State(store): State<Store>,
    Path((index, name)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    let view = alias_view(
        &store,
        Some(&index),
        Some(&name),
        p.get("expand_wildcards").map(|v| v.as_str()),
    );
    let missing = alias_names_missing(&store, Some(&index), Some(&name));
    if !missing.is_empty() {
        return aliases_missing_response(&missing, &view);
    }
    respond(&p, view)
}

pub async fn index_alias_head(
    State(store): State<Store>,
    Path((index, name)): Path<(String, String)>,
) -> Response {
    let view = alias_view(&store, Some(&index), Some(&name), None);
    let any = view
        .as_object()
        .map(|o| {
            o.values().any(|v| {
                v.get("aliases").and_then(|a| a.as_object()).map(|a| !a.is_empty()).unwrap_or(false)
            })
        })
        .unwrap_or(false);
    if any { StatusCode::OK.into_response() } else { StatusCode::NOT_FOUND.into_response() }
}

pub async fn exists_alias(State(store): State<Store>, path: Option<Path<Vec<String>>>) -> Response {
    let parts = path.map(|Path(v)| v).unwrap_or_default();
    let (idx, name) = match parts.len() {
        0 => (None, None),
        1 => (None, Some(parts[0].clone())),
        _ => (Some(parts[0].clone()), Some(parts[1].clone())),
    };
    let view = alias_view(&store, idx.as_deref(), name.as_deref(), None);
    let any = view
        .as_object()
        .map(|o| {
            o.values().any(|v| {
                v.get("aliases").and_then(|a| a.as_object()).map(|a| !a.is_empty()).unwrap_or(false)
            })
        })
        .unwrap_or(false);
    if any { StatusCode::OK.into_response() } else { StatusCode::NOT_FOUND.into_response() }
}

/// Create or replace an alias.
///
/// The index and the alias name may each arrive in the path or in the body,
/// which is four spellings of the same request.
pub(crate) async fn put_alias_inner(
    store: Store,
    index: Option<String>,
    name: Option<String>,
    p: Params,
    body: String,
) -> Response {
    let mut def: Value = parse_body(&body).unwrap_or_else(|_| json!({}));
    if !def.is_object() {
        def = json!({});
    }
    let from_body = |keys: &[&str]| -> Option<String> {
        let o = def.as_object()?;
        for k in keys {
            match o.get(*k) {
                Some(Value::String(s)) => return Some(s.clone()),
                Some(Value::Array(a)) => {
                    let joined: Vec<String> =
                        a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
                    if !joined.is_empty() {
                        return Some(joined.join(","));
                    }
                }
                _ => {}
            }
        }
        None
    };
    // what the body names wins over what the path names, for the index and
    // for the alias alike
    let index = from_body(&["index", "indices"]).or_else(|| index.filter(|s| !s.is_empty()));
    let name = from_body(&["alias", "aliases"]).or_else(|| name.filter(|s| !s.is_empty()));

    let (Some(index), Some(name)) = (index, name) else {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: index is missing;2: alias is missing;",
        );
    };
    if let Some(o) = def.as_object() {
        for key in o.keys() {
            let key: &str = key;
            if !ALIAS_ADDRESSING.contains(&key) && !ALIAS_OPTIONS.contains(&key) {
                return err(
                    StatusCode::BAD_REQUEST,
                    "x_content_parse_exception",
                    format!("unknown field [{key}]"),
                );
            }
        }
    }
    // an alias is a name for indices, so it can be neither a pattern nor the
    // name an index already answers to
    if name.contains('*') || name.contains(',') {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_alias_name_exception",
            format!("Invalid alias name [{name}]"),
        );
    }
    if store.names().contains(&name) {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_alias_name_exception",
            format!(
                "Invalid alias name [{name}]: an index or data stream exists with the same name as the alias"
            ),
        );
    }
    let targets = store.resolve(&index);
    if targets.is_empty() {
        return no_such_index(&index);
    }
    if let Some(o) = def.as_object_mut() {
        for k in ALIAS_ADDRESSING {
            o.remove(*k);
        }
    }
    if let Some(refusal) = two_write_indices(&store, &name, &def, &targets) {
        return refusal;
    }
    for n in targets {
        if let Some(st) = store.get(&n) {
            let mut g = st.write();
            g.aliases.insert(name.clone(), crate::store::normalize_alias(&def));
            g.save_meta();
        }
    }
    respond(&p, json!({"acknowledged": true}))
}

/// Whether adding this alias to these indices would leave more than one of
/// them as its write index.
///
/// An alias has one index writes go to, or none: with two the destination is
/// whichever the resolution happens to list first, which is a write landing
/// in an index nobody meant. The reference refuses the second one, in these
/// words, and nothing here did.
pub(crate) fn two_write_indices(
    store: &Store,
    alias: &str,
    def: &Value,
    adding_to: &[String],
) -> Option<Response> {
    let asked_write = def.get("is_write_index").and_then(|v| v.as_bool()).unwrap_or(false);
    if !asked_write {
        return None;
    }
    let mut writers: Vec<String> = store
        .resolve(alias)
        .into_iter()
        .filter(|n| !adding_to.contains(n))
        .filter(|n| {
            store
                .get(n)
                .map(|st| {
                    st.read()
                        .aliases
                        .get(alias)
                        .and_then(|d| d.get("is_write_index"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        })
        .collect();
    writers.extend(adding_to.iter().cloned());
    writers.sort();
    writers.dedup();
    (writers.len() > 1).then(|| {
        err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("alias [{alias}] has more than one write index [{}]", writers.join(",")),
        )
    })
}

pub async fn put_alias(
    State(store): State<Store>,
    Path((index, name)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    put_alias_inner(store, Some(index), Some(name), p, body).await
}

/// `PUT /{index}/_alias` -- the alias name comes from the body.
pub async fn put_alias_on_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    put_alias_inner(store, Some(index), None, p, body).await
}

/// `PUT /_alias/{name}` -- the indices come from the body.
pub async fn put_alias_named(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    put_alias_inner(store, None, Some(name), p, body).await
}

/// `PUT /_alias` -- both come from the body.
pub async fn put_alias_body(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    put_alias_inner(store, None, None, p, body).await
}

pub async fn delete_alias(
    State(store): State<Store>,
    Path((index, name)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    let targets = store.resolve(&index);
    if targets.is_empty() {
        return no_such_index(&index);
    }
    let mut removed = false;
    for n in targets {
        if let Some(st) = store.get(&n) {
            for pat in name.split(',') {
                let pat = pat.trim();
                let mut g = st.write();
                // `_all` names every alias the index carries
                let hits: Vec<String> = if pat == "_all" {
                    g.aliases.keys().cloned().collect()
                } else {
                    let re = crate::store::wildcard_to_regex(pat);
                    g.aliases.keys().filter(|a| re.is_match(a)).cloned().collect()
                };
                for h in hits {
                    g.aliases.remove(&h);
                    removed = true;
                }
                g.save_meta();
            }
        }
    }
    if !removed {
        return aliases_missing(&name);
    }
    respond(&p, json!({"acknowledged": true}))
}

pub async fn update_aliases(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let actions = body.get("actions").and_then(|a| a.as_array()).cloned().unwrap_or_default();
    if actions.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: Must specify at least one alias action;",
        );
    }
    // `must_exist` on a remove turns "nothing to do" into an error, and every
    // action is checked before any of them is reported
    let mut missing_required: Vec<String> = Vec::new();
    for action in actions {
        let Some((verb, spec)) = action.as_object().and_then(|o| o.iter().next()) else { continue };
        let indices: Vec<String> = spec
            .get("index")
            .and_then(|v| v.as_str())
            .map(|s| store.resolve(s))
            .or_else(|| {
                spec.get("indices").and_then(|v| v.as_array()).map(|a| {
                    a.iter().filter_map(|x| x.as_str()).flat_map(|x| store.resolve(x)).collect()
                })
            })
            .unwrap_or_default();
        let names: Vec<String> = spec
            .get("alias")
            .and_then(|v| v.as_str())
            .map(|s| vec![s.to_string()])
            .or_else(|| {
                spec.get("aliases")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            })
            .unwrap_or_default();
        // an explicit empty list is a request that names nothing
        if spec.get("aliases").and_then(|v| v.as_array()).map(|a| a.is_empty()).unwrap_or(false) {
            return err(
                StatusCode::BAD_REQUEST,
                "action_request_validation_exception",
                "Validation Failed: 1: [aliases] can't be empty;",
            );
        }
        if indices.is_empty() {
            let want = spec.get("index").and_then(|v| v.as_str()).unwrap_or("");
            return no_such_index(want);
        }
        // one write index per alias, checked before any of them is written
        if verb == "add" {
            for a in &names {
                let mut def = spec.clone();
                if let Some(o) = def.as_object_mut() {
                    for k in ["index", "indices", "alias", "aliases"] {
                        o.remove(k);
                    }
                }
                if let Some(refusal) = two_write_indices(&store, a, &def, &indices) {
                    return refusal;
                }
            }
        }
        // a remove that removed nothing is an error, whether or not the caller
        // asked for must_exist -- there was nothing there to act on
        let mut removed_any = false;
        for i in &indices {
            let Some(st) = store.get(i) else { continue };
            let mut g = st.write();
            for a in &names {
                match verb.as_str() {
                    "add" => {
                        let mut def = spec.clone();
                        if let Some(o) = def.as_object_mut() {
                            o.remove("index");
                            o.remove("indices");
                            o.remove("alias");
                            o.remove("aliases");
                        }
                        g.aliases.insert(a.clone(), crate::store::normalize_alias(&def));
                        g.save_meta();
                    }
                    "remove" => {
                        let re = crate::store::wildcard_to_regex(a);
                        let hits: Vec<String> =
                            g.aliases.keys().filter(|x| re.is_match(x)).cloned().collect();
                        removed_any |= !hits.is_empty();
                        for h in hits {
                            g.aliases.remove(&h);
                        }
                        g.save_meta();
                    }
                    "remove_index" => {}
                    _ => {}
                }
            }
        }
        if verb == "remove_index" {
            // `remove_index` deletes the index a name stands for, and a name
            // that stands for several is not one of them: the delete endpoint
            // refuses exactly this, and here it deleted every index behind the
            // alias -- `{"remove_index": {"index": "*"}}` deleted the cluster.
            // the name as it was asked for, before it was resolved: the
            // resolved list is concrete indices, and an alias among them is
            // exactly what has to be refused
            let asked: Vec<String> = spec
                .get("index")
                .and_then(|v| v.as_str())
                .map(|s| vec![s.to_string()])
                .or_else(|| {
                    spec.get("indices").and_then(|v| v.as_array()).map(|a| {
                        a.iter().filter_map(|x| x.as_str()).map(|x| x.to_string()).collect()
                    })
                })
                .unwrap_or_default();
            for one in &asked {
                // a name that is both -- an index called `test` and, added in
                // this same request, an alias called `test` -- is the index:
                // the reference removes it, and refusing it broke
                // `indices.update_aliases/30_remove_index_and_replace_with_alias`
                let names_an_index = store.get(one).is_some();
                if (store.is_alias(one) && !names_an_index) || one.contains('*') {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        format!(
                            "The provided expression [{one}] matches an alias or a pattern, \
                             specify the corresponding concrete indices instead."
                        ),
                    );
                }
            }
            for i in &indices {
                store.delete(i);
            }
        }
        // must_exist spelled out decides it either way; left unsaid, a remove
        // that matched nothing at all is still an error
        let complain = spec.get("must_exist").and_then(|v| v.as_bool()).unwrap_or(true);
        if verb == "remove" && !removed_any && !names.is_empty() && complain {
            missing_required.extend(names.iter().cloned());
        }
    }
    if !missing_required.is_empty() {
        missing_required.sort();
        missing_required.dedup();
        return aliases_missing(&missing_required.join(","));
    }
    respond(&p, json!({"acknowledged": true}))
}

/// `aliases [x] missing`, with the resource it names, as the reference says it.
fn aliases_missing(names: &str) -> Response {
    let reason = format!("aliases [{names}] missing");
    let cause = json!({"type": "aliases_not_found_exception", "reason": reason,
                       "resource.type": "aliases", "resource.id": names});
    let mut error = cause.clone();
    error["root_cause"] = json!([cause]);
    (StatusCode::NOT_FOUND, axum::Json(json!({"error": error, "status": 404}))).into_response()
}
