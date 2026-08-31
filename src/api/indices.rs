//! An index as a thing: made, opened, closed, resized, rolled over, deleted.

use super::*;

pub(crate) fn shards() -> Value {
    json!({"total": 2, "successful": 1, "failed": 0})
}

/// A shard tally over a set of indices: every shard they declare, all of them
/// on the one node and so all of them successful.
pub(crate) fn shards_over(store: &Store, names: &[String]) -> Value {
    let total: u64 = names
        .iter()
        .filter_map(|n| store.get(n))
        .map(|st| st.read().numeric_setting("number_of_shards").unwrap_or(1).max(1))
        .sum();
    json!({"total": total, "successful": total, "failed": 0})
}

/// The shards of one index, which is what a write to it reports.
pub(crate) fn shards_of(st: &IdxState) -> Value {
    let n = st.numeric_setting("number_of_shards").unwrap_or(1).max(1);
    json!({"total": n, "successful": n, "failed": 0})
}

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

/// `_split`, `_shrink` and `_clone` -- make a new index out of an existing
/// one.
///
/// All three mean the same thing to an engine holding one shard per index: a
/// copy of every document under whatever settings the target is given. What
/// differs between them is only which shard counts are allowed, and that is
/// checked rather than acted on.
pub async fn resize_index(
    State(store): State<Store>,
    Path((source, target)): Path<(String, String)>,
    Query(p): Query<Params>,
    body: String,
    kind: &'static str,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let Some(src) = store.get(&source) else { return no_such_index(&source) };
    if store.exists(&target) {
        return err(
            StatusCode::BAD_REQUEST,
            "resource_already_exists_exception",
            format!("index [{target}] already exists"),
        );
    }
    let setting = |k: &str| -> Option<Value> {
        body.pointer(&format!("/settings/index/{k}"))
            .or_else(|| body.pointer(&format!("/settings/index.{k}")))
            .cloned()
    };
    let num = |k: &str| {
        setting(k).and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
    };
    // a target that cannot be written to would be created and then be
    // unusable, so those blocks are refused on the way in rather than left to
    // fail the copy
    for blocked in ["blocks.read_only", "blocks.metadata", "blocks.read_only_allow_delete"] {
        let set = setting(blocked).map(|v| v == json!(true) || v == json!("true")).unwrap_or(false);
        if set {
            return err(
                StatusCode::BAD_REQUEST,
                "action_request_validation_exception",
                format!(
                    "Validation Failed: 1: illegal value can not be set [index.{blocked}] on \
                     the target index;"
                ),
            );
        }
    }
    let from = src.read().numeric_setting("number_of_shards").unwrap_or(1) as i64;
    if let Some(to) = num("number_of_shards") {
        // a split has to multiply the shard count, a shrink to divide it
        match kind {
            "split" => {
                if setting("number_of_routing_shards").is_some() {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        "cannot provide index.number_of_routing_shards on resize",
                    );
                }
                if to <= from || to % from != 0 {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "illegal_state_exception",
                        format!("the number of source shards [{from}] must be a factor of [{to}]"),
                    );
                }
            }
            "shrink" => {
                if to >= from || from % to != 0 {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "illegal_state_exception",
                        format!(
                            "the number of source shards [{from}] must be a multiple of [{to}]"
                        ),
                    );
                }
            }
            _ => {
                if to != from {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        format!("the number of source shards [{from}] must be equal to [{to}]"),
                    );
                }
            }
        }
    }
    // the source has to be held still first: a copy taken while writes are
    // landing would not be a copy of anything in particular
    {
        let g = src.read();
        // a read-only source cannot be resized at all: the operation has to
        // write to it, not only read from it. The request may lift the block
        // as part of the same call, which is how a caller unwinds one.
        let lifted = matches!(setting("blocks.read_only"), Some(Value::Null))
            || setting("blocks.read_only")
                .map(|v| v == json!(false) || v == json!("false"))
                .unwrap_or(false);
        if !lifted && g.setting("blocks.read_only").as_deref() == Some("true") {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("index [{source}] blocked by: [FORBIDDEN/5/index read-only (api)];"),
            );
        }
        if g.setting("blocks.write").as_deref() != Some("true") {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_state_exception",
                format!(
                    "index {source} must be read-only to resize index. use \
                     \"index.blocks.write=true\""
                ),
            );
        }
    }
    // the target starts from the source's shape, with what the request says
    // laid over it
    let (mapping, aliases, ids, inherited) = {
        let g = src.read();
        let ids: Vec<String> = g.all_ids();
        // the new index is the old one under another name, so it starts from
        // the source's settings and takes the request's over the top
        let mut inherited = g.effective_settings();
        if let Some(o) = inherited.pointer_mut("/index").and_then(|v| v.as_object_mut()) {
            o.retain(|k, _| !matches!(k.as_str(), "provided_name" | "uuid" | "creation_date"));
        }
        (g.mapping.raw.clone(), body.get("aliases").cloned(), ids, inherited)
    };
    let mut create = json!({"mappings": mapping});
    // settings always follow the index now; asking for them not to is asking
    // for behaviour that no longer exists
    if p.get("copy_settings").map(|v| v == "false").unwrap_or(false) {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "parameter [copy_settings] can not be explicitly set to [false]",
        );
    }
    let copy = true;
    let mut settings = if copy { inherited } else { json!({}) };
    if let Some(set) = body.get("settings") {
        crate::store::deep_merge(&mut settings, set);
    }
    // `max_shard_size` asks for as many shards as the data needs rather than
    // for a number; what this much data needs is one
    let asked_max_size = p.contains_key("max_shard_size") || body.get("max_shard_size").is_some();
    if asked_max_size && num("number_of_shards").is_none() {
        if let Some(o) = settings.pointer_mut("/index").and_then(|v| v.as_object_mut()) {
            o.insert("number_of_shards".into(), json!("1"));
        }
    }
    // the block that held the source still is carried over, but not until the
    // documents are: the copy goes through the write path, which the block
    // would otherwise refuse
    let mut held_back = serde_json::Map::new();
    if let Some(o) = settings.pointer_mut("/index").and_then(|v| v.as_object_mut()) {
        let blocked: Vec<String> = o.keys().filter(|k| k.starts_with("blocks")).cloned().collect();
        for k in blocked {
            if let Some(v) = o.remove(&k) {
                held_back.insert(k, v);
            }
        }
    }
    if settings.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
        create["settings"] = settings;
    }
    if let Some(a) = aliases {
        create["aliases"] = a;
    }
    if let Err(e) = store.create(&target, &create) {
        return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string());
    }
    let Some(dst) = store.get(&target) else { return no_such_index(&target) };
    for id in ids {
        let doc = { src.read().pending.get(&id).cloned().flatten() };
        let source_doc = match doc {
            Some(raw) => serde_json::from_str(&raw).ok(),
            None => read_source(&src.read(), &id),
        };
        let Some(v) = source_doc else { continue };
        let mut g = dst.write();
        let _ = write_doc(&mut g, &id, v, "index");
    }
    {
        let mut g = dst.write();
        let _ = g.refresh();
        if !held_back.is_empty() {
            let mut set = g.settings.clone();
            if !set.is_object() {
                set = json!({});
            }
            let slot = entry_of(&mut set, "index", || json!({}));
            crate::store::deep_merge(slot, &Value::Object(held_back));
            g.settings = set;
            g.save_meta();
        }
    }
    // `wait_for_completion=false` asks for the work to be tracked rather than
    // waited on; the copy is already done, so the task is a finished one
    if p.get("wait_for_completion").map(|v| v == "false").unwrap_or(false) {
        return respond(
            &p,
            json!({
                "task": format!("node-0:{kind} from [{source}] to [{target}]")
            }),
        );
    }
    respond(
        &p,
        json!({
            "acknowledged": true, "shards_acknowledged": true, "index": target
        }),
    )
}

pub async fn split_index(
    state: State<Store>,
    path: Path<(String, String)>,
    q: Query<Params>,
    body: String,
) -> Response {
    resize_index(state, path, q, body, "split").await
}

pub async fn shrink_index(
    state: State<Store>,
    path: Path<(String, String)>,
    q: Query<Params>,
    body: String,
) -> Response {
    resize_index(state, path, q, body, "shrink").await
}

pub async fn clone_index(
    state: State<Store>,
    path: Path<(String, String)>,
    q: Query<Params>,
    body: String,
) -> Response {
    resize_index(state, path, q, body, "clone").await
}

/// The name that follows this one in a rolled-over series.
///
/// A name ending in digits carries on from that number, padded to six places,
/// which is how a series started by hand as `logs-1` becomes `logs-000002`.
pub(crate) fn next_rollover_name(current: &str) -> Option<String> {
    let digits = current.len() - current.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    let (stem, num) = current.split_at(current.len() - digits);
    let next: u64 = num.parse::<u64>().ok()? + 1;
    Some(format!("{stem}{next:06}"))
}

/// `_rollover` -- start a new index behind an alias when the one it points at
/// has had enough.
pub async fn rollover(
    State(store): State<Store>,
    path: Path<Vec<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let parts = path.0;
    let alias = parts.first().cloned().unwrap_or_default();
    let asked_name = parts.get(1).cloned();
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let dry_run = p.get("dry_run").map(|v| v != "false").unwrap_or(false);

    // the alias has to name exactly one index, or there is no one index to
    // roll over from
    let behind = store.resolve(&alias);
    let Some(old) = behind.first().cloned().filter(|_| behind.len() == 1) else {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("source alias [{alias}] does not point to a single index"),
        );
    };
    let Some(src) = store.get(&old) else { return no_such_index(&old) };
    let docs = src.read().reader.searcher().num_docs();

    // every condition is reported with whether it was met, met or not
    let mut conditions = serde_json::Map::new();
    let mut met = false;
    if let Some(o) = body.get("conditions").and_then(|v| v.as_object()) {
        for (name, want) in o {
            let text = want.as_str().map(|s| s.to_string()).unwrap_or_else(|| want.to_string());
            let hit = match name.as_str() {
                "max_docs" => docs >= want.as_u64().unwrap_or(u64::MAX),
                // the size an index has been given, against the size asked
                // about -- an index this young meets no age condition
                "max_size" | "max_primary_shard_size" => parse_size(&text)
                    .map(|limit| {
                        src.read().bytes.load(std::sync::atomic::Ordering::Relaxed) >= limit
                    })
                    .unwrap_or(false),
                "max_age" => false,
                _ => false,
            };
            met = met || hit;
            conditions.insert(format!("[{name}: {text}]"), json!(hit));
        }
    }

    let Some(new_index) = asked_name.or_else(|| next_rollover_name(&old)) else {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("index name [{old}] does not match pattern '^.*-\\d+$'"),
        );
    };
    // the characters a name may not carry, which a caller supplying one of
    // their own can still get wrong
    if new_index.chars().any(|c| " \"*\\<|,>/?".contains(c)) {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_index_name_exception",
            format!(
                "Invalid index name [{new_index}], must not contain the following characters [ , \", *, \\, <, |, ,, >, /, ?]"
            ),
        );
    }
    if store.exists(&new_index) {
        return err(
            StatusCode::BAD_REQUEST,
            "resource_already_exists_exception",
            format!("index [{new_index}] already exists"),
        );
    }

    let rolled = met && !dry_run;
    if rolled {
        let create = body
            .get("mappings")
            .or_else(|| body.get("settings"))
            .map(|_| {
                let mut c = json!({});
                for k in ["settings", "mappings", "aliases"] {
                    if let Some(v) = body.get(k) {
                        c[k] = v.clone();
                    }
                }
                c
            })
            .unwrap_or_else(|| json!({}));
        if let Err(e) = store.create(&new_index, &create) {
            return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", e.to_string());
        }
        // the alias moves; the old index keeps whatever else pointed at it
        let def = { src.read().aliases.get(&alias).cloned().unwrap_or_else(|| json!({})) };
        if let Some(st) = store.get(&new_index) {
            st.write().aliases.insert(alias.clone(), def);
        }
        src.write().aliases.remove(&alias);
    }
    respond(
        &p,
        json!({
            "acknowledged": true,
            "shards_acknowledged": rolled,
            "old_index": old,
            "new_index": new_index,
            "rolled_over": rolled,
            "dry_run": dry_run,
            "conditions": Value::Object(conditions),
        }),
    )
}

/// `_shard_stores` -- where each shard's copies are.
///
/// One node holds one copy of each shard, and it is the primary.
pub async fn shard_stores(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let names = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
    let allow_none = p.get("allow_no_indices").map(|v| v != "false").unwrap_or(true);
    if names.is_empty() && !allow_none {
        return no_such_index(if expr.is_empty() { "_all" } else { &expr });
    }
    // by default only the shards that are missing a copy are listed, and none
    // here ever are
    let status: Vec<&str> = p
        .get("status")
        .map(|v| v.split(',').map(|s| s.trim()).collect())
        .unwrap_or_else(|| vec!["yellow", "red"]);
    let listed = status.iter().any(|s| matches!(*s, "green" | "all"));
    let mut indices = serde_json::Map::new();
    if listed {
        for n in names {
            let Some(st) = store.get(&n) else { continue };
            let g = st.read();
            let shards = g.numeric_setting("number_of_shards").unwrap_or(1).max(1);
            let mut per = serde_json::Map::new();
            for i in 0..shards {
                per.insert(
                    i.to_string(),
                    json!({"stores": [{
                        "node-0": {
                            "name": "boostsearch", "ephemeral_id": "_na_",
                            "transport_address": "127.0.0.1:9300", "attributes": {},
                        },
                        "allocation_id": "_na_",
                        "allocation": "primary",
                    }]}),
                );
            }
            indices.insert(g.name.clone(), json!({"shards": Value::Object(per)}));
        }
    }
    respond(&p, json!({"indices": Value::Object(indices)}))
}

/// `_resolve/index` -- what a name or pattern actually reaches.
pub async fn resolve_index(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let mut indices = Vec::new();
    let mut aliases: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    // a pattern reaches the states `expand_wildcards` names, which is open
    // ones unless the caller says otherwise
    let states = p.get("expand_wildcards").map(|v| v.as_str()).unwrap_or("open");
    let want_open = states.split(',').any(|w| matches!(w.trim(), "open" | "all"));
    let want_closed = states.split(',').any(|w| matches!(w.trim(), "closed" | "all"));
    let mut names = store.names();
    names.sort();
    for n in names {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        let mut own: Vec<String> = g.aliases.keys().cloned().collect();
        own.sort();
        // an alias names every index behind it, whatever state each is in --
        // closing one does not take it out from behind its alias
        for a in &own {
            aliases.entry(a.clone()).or_default().push(g.name.clone());
        }
        // the index listing itself only reaches the states asked for
        if (g.closed && !want_closed) || (!g.closed && !want_open) {
            continue;
        }
        let hit = name.split(',').any(|pat| {
            let pat = pat.trim();
            pat == "*"
                || pat == "_all"
                || pat == g.name
                || crate::store::glob_match(pat, &g.name)
                || own.iter().any(|a| a == pat || crate::store::glob_match(pat, a))
        });
        if hit {
            indices.push(json!({
                "name": g.name,
                "aliases": own,
                "attributes": [if g.closed { "closed" } else { "open" }],
            }));
        }
    }
    let alias_list: Vec<Value> = aliases
        .into_iter()
        .filter(|(a, _)| {
            name.split(',').any(|pat| {
                let pat = pat.trim();
                pat == "*" || pat == "_all" || pat == a || crate::store::glob_match(pat, a)
            })
        })
        .map(|(a, mut idx)| {
            idx.sort();
            json!({"name": a, "indices": idx})
        })
        .collect();
    respond(
        &p,
        json!({
            "indices": indices, "aliases": alias_list, "data_streams": [],
        }),
    )
}

/// `_block/{block}` -- hold an index still without closing it.
pub async fn add_block(
    State(store): State<Store>,
    Path((index, block)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    let targets = store.resolve(&index);
    if targets.is_empty() {
        return no_such_index(&index);
    }
    let mut blocked = Vec::new();
    for n in &targets {
        let Some(st) = store.get(n) else { continue };
        let mut g = st.write();
        let mut settings = g.settings.clone();
        if !settings.is_object() {
            settings = json!({});
        }
        let slot = entry_of(&mut settings, "index", || json!({}));
        crate::store::deep_merge(slot, &json!({format!("blocks.{block}"): "true"}));
        g.settings = settings;
        g.save_meta();
        blocked.push(json!({"name": g.name.clone(), "blocked": true}));
    }
    respond(
        &p,
        json!({
            "acknowledged": true, "shards_acknowledged": true, "indices": blocked
        }),
    )
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
            for n in store.names() {
                if crate::store::glob_match(part, &n) && !targets.contains(&n) {
                    targets.push(n);
                }
            }
        } else if store.is_alias(part) {
            continue;
        } else {
            let found = store.resolve(part);
            if found.is_empty() {
                missing.get_or_insert_with(|| part.to_string());
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

/// `_flush` writes what is buffered and makes it searchable.
///
/// The distinction OpenSearch draws is between committing to disk and making
/// documents visible; here committing does both, so a flush is a refresh that
/// also settles the writer.
pub async fn flush(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(_p): Query<Params>,
) -> Response {
    // a forced flush has to be allowed to wait for one already running, or it
    // would have to refuse to do the thing it was asked for
    if _p.get("force").map(|v| v != "false").unwrap_or(false)
        && _p.get("wait_if_ongoing").map(|v| v == "false").unwrap_or(false)
    {
        return err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            "Validation Failed: 1: wait_if_ongoing must be true for a force flush;",
        );
    }
    let targets = match index {
        Some(Path(i)) => {
            let t = store.resolve(&i);
            if t.is_empty() {
                return no_such_index(&i);
            }
            t
        }
        None => store.names(),
    };
    let tally = shards_over(&store, &targets);
    for n in targets {
        if let Some(st) = store.get(&n) {
            let mut g = st.write();
            let _ = g.refresh();
            g.flushes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    axum::Json(json!({"_shards": tally})).into_response()
}

pub async fn refresh_all(State(store): State<Store>) -> Response {
    let names = store.names();
    for n in &names {
        if let Some(st) = store.get(n) {
            let _ = st.write().refresh();
        }
    }
    axum::Json(json!({"_shards": shards_over(&store, &names)})).into_response()
}

pub async fn refresh_index(State(store): State<Store>, Path(index): Path<String>) -> Response {
    let targets = store.resolve(&index);
    // a pattern that reaches nothing has nothing to refresh, which is not an
    // error; a name given outright must be there
    if targets.is_empty() && !index.contains('*') && index != "_all" && !index.is_empty() {
        return no_such_index(&index);
    }
    let tally = shards_over(&store, &targets);
    for n in targets {
        if let Some(st) = store.get(&n) {
            let _ = st.write().refresh();
        }
    }
    axum::Json(json!({"_shards": tally})).into_response()
}

/// Defaults a type carries even when the request did not spell them out.
pub(crate) fn add_type_defaults(node: &mut Value) {
    let Some(obj) = node.as_object_mut() else { return };
    if obj.get("type").and_then(|t| t.as_str()) == Some("wildcard")
        && !obj.contains_key("doc_values")
    {
        obj.insert("doc_values".into(), json!(true));
    }
    for key in ["properties", "fields"] {
        if let Some(children) = obj.get_mut(key).and_then(|c| c.as_object_mut()) {
            for (_, child) in children.iter_mut() {
                add_type_defaults(child);
            }
        }
    }
}

/// Total shard count across the resolved indices, primaries plus replicas.
pub fn shard_total(store: &Store, names: &[String]) -> u64 {
    names
        .iter()
        .filter_map(|n| store.get(n))
        .map(|st| {
            let g = st.read();
            let s = g.effective_settings();
            let num = |k: &str, d: u64| {
                s.pointer(&format!("/index/{k}"))
                    .and_then(|v| v.as_str().and_then(|x| x.parse().ok()).or_else(|| v.as_u64()))
                    .unwrap_or(d)
            };
            num("number_of_shards", 1) * (1 + num("number_of_replicas", 1))
        })
        .sum()
}

/// `_forcemerge` collapses segments. Fewer segments means less per-segment setup
/// on every search, which matters most for aggregations: each one opens columns
/// and builds its own intermediate result per segment before they are merged.
pub async fn force_merge(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_else(|| "_all".into());
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" {
        return no_such_index(&expr);
    }
    // a merge reaches every copy of a shard unless told to keep to the
    // primaries, and a replica is a copy this node does not hold
    let primary_only = p.get("primary_only").map(|v| v != "false").unwrap_or(false);
    let touched: u64 = targets
        .iter()
        .filter_map(|n| store.get(n))
        .map(|st| {
            let g = st.read();
            let shards = g.numeric_setting("number_of_shards").unwrap_or(1).max(1);
            let copies = if primary_only {
                1
            } else {
                1 + g.numeric_setting("number_of_replicas").unwrap_or(0)
            };
            shards * copies
        })
        .sum();
    let max_segments: usize =
        p.get("max_num_segments").and_then(|v| v.parse().ok()).unwrap_or(1).max(1);

    for name in targets {
        let Some(st) = store.get(&name) else { continue };
        let mut g = st.write();
        if g.refresh().is_err() {
            continue;
        }
        loop {
            let ids: Vec<boostcore::index::SegmentId> = g
                .index
                .searchable_segment_metas()
                .unwrap_or_default()
                .iter()
                .map(|m| m.id())
                .collect();
            if ids.len() <= max_segments {
                break;
            }
            // merge the whole set down in one step; BoostCore handles the rest
            let take = ids.len() - max_segments + 1;
            let batch: Vec<_> = ids.into_iter().take(take).collect();
            let merged = match g.writer() {
                Ok(w) => w.merge(&batch).wait().is_ok(),
                Err(_) => false,
            };
            if !merged {
                break;
            }
            let _ = g.refresh();
        }
    }
    respond(
        &p,
        json!({
            "_shards": {"total": touched, "successful": touched, "failed": 0}
        }),
    )
}

/// `_segments` -- what each shard is made of.
///
/// One shard per index here, and BoostCore names its segments by ordinal, so
/// they are reported as `_0`, `_1` and so on to match the shape the API has.
pub async fn segments(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let targets = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
    let allow_none = p.get("allow_no_indices").map(|v| v != "false").unwrap_or(true);
    if targets.is_empty() {
        if !allow_none || (!expr.is_empty() && !expr.contains('*') && !store.exists(&expr)) {
            return no_such_index(&expr);
        }
        return respond(
            &p,
            json!({
                "_shards": {"total": 0, "successful": 0, "failed": 0},
                "indices": {},
            }),
        );
    }
    let mut indices = serde_json::Map::new();
    let mut total = 0u64;
    for n in &targets {
        let Some(st) = store.get(n) else { continue };
        let g = st.read();
        if g.closed {
            // a closed index has nothing to report; the caller decides whether
            // that is an error or simply nothing
            if p.get("ignore_unavailable").map(|v| v != "false").unwrap_or(false) {
                continue;
            }
            return err(
                StatusCode::BAD_REQUEST,
                "index_closed_exception",
                format!("closed index [{n}]"),
            );
        }
        let searcher = g.reader.searcher();
        let mut segs = serde_json::Map::new();
        for (i, reader) in searcher.segment_readers().iter().enumerate() {
            segs.insert(
                format!("_{i}"),
                json!({
                    "generation": i,
                    "num_docs": reader.num_docs(),
                    "deleted_docs": reader.num_deleted_docs(),
                    "size_in_bytes": 0,
                    "memory_in_bytes": 0,
                    "committed": true,
                    "search": true,
                    "version": "9.0.0",
                    "compound": true,
                    "attributes": {},
                }),
            );
        }
        total += 1;
        indices.insert(
            n.clone(),
            json!({"shards": {"0": [{
                "routing": {"state": "STARTED", "primary": true, "node": "boostsearch"},
                "num_committed_segments": segs.len(),
                "num_search_segments": segs.len(),
                "segments": Value::Object(segs),
            }]}}),
        );
    }
    respond(
        &p,
        json!({
            "_shards": {"total": total, "successful": total, "failed": 0},
            "indices": Value::Object(indices),
        }),
    )
}

/// `_recovery` -- how each shard came to be where it is.
///
/// A shard here is created empty and is immediately done, so every recovery
/// is an EMPTY_STORE that finished, with nothing copied from anywhere.
pub async fn indices_recovery(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let names = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
    if !expr.is_empty() && !ignore_unavailable(&p) {
        for part in expr.split(',').map(|n| n.trim()).filter(|n| !n.contains('*')) {
            if store.resolve(part).is_empty() {
                return no_such_index(part);
            }
        }
    }
    let mut out = serde_json::Map::new();
    for n in names {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        let existing = g.reader.searcher().num_docs() > 0 || g.closed;
        out.insert(
            g.name.clone(),
            json!({"shards": [{
                "id": 0,
                "type": if g.restored {
                    "SNAPSHOT"
                } else if existing {
                    "EXISTING_STORE"
                } else {
                    "EMPTY_STORE"
                },
                "stage": "DONE",
                "primary": true,
                "start_time": "2020-01-01T00:00:00.000Z",
                "start_time_in_millis": 1_577_836_800_000u64,
                "stop_time": "2020-01-01T00:00:00.000Z",
                "stop_time_in_millis": 1_577_836_800_000u64,
                "total_time": "0s",
                "total_time_in_millis": 0,
                "source": {},
                "target": {
                    "id": "node-0", "host": "127.0.0.1", "transport_address": "127.0.0.1:9300",
                    "ip": "127.0.0.1", "name": "boostsearch",
                },
                "index": {
                    "size": {
                        "total": if g.restored { "1kb" } else { "0b" },
                        "total_in_bytes": if g.restored { 1024 } else { 0 },
                        "reused": "0b", "reused_in_bytes": 0,
                        "recovered": if g.restored { "1kb" } else { "0b" },
                        "recovered_in_bytes": if g.restored { 1024 } else { 0 },
                        "percent": "100.0%",
                    },
                    "files": {
                        "total": if g.restored { 1 } else { 0 },
                        "reused": 0,
                        "recovered": if g.restored { 1 } else { 0 },
                        "percent": "100.0%",
                        "details": [],
                    },
                    "total_time": "0s", "total_time_in_millis": 0,
                    "source_throttle_time": "0s", "source_throttle_time_in_millis": 0,
                    "target_throttle_time": "0s", "target_throttle_time_in_millis": 0,
                },
                "translog": {
                    "recovered": 0, "total": 0, "percent": "100.0%",
                    "total_on_start": 0, "total_time": "0s", "total_time_in_millis": 0,
                },
                "verify_index": {
                    "check_index_time": "0s", "check_index_time_in_millis": 0,
                    "total_time": "0s", "total_time_in_millis": 0,
                },
            }]}),
        );
    }
    respond(&p, Value::Object(out))
}

/// `_upgrade` -- there is one segment format and it is the current one, so an
/// upgrade has nothing to do but say which it is.
pub async fn indices_upgrade(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let names = if expr.is_empty() { store.names() } else { store.resolve(&expr) };
    if !expr.is_empty() && !ignore_unavailable(&p) {
        for part in expr.split(',').map(|n| n.trim()).filter(|n| !n.contains('*')) {
            if store.resolve(part).is_empty() {
                return no_such_index(part);
            }
        }
    }
    let tally = shards_over(&store, &names);
    let mut upgraded = serde_json::Map::new();
    for n in names {
        let Some(st) = store.get(&n) else { continue };
        let g = st.read();
        upgraded.insert(
            g.name.clone(),
            json!({
                "upgrade_version": "3.0.0",
                "oldest_lucene_segment_version": "9.0.0",
            }),
        );
    }
    respond(&p, json!({"_shards": tally, "upgraded_indices": Value::Object(upgraded)}))
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

pub async fn close_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let targets = store.resolve(&index);
    if targets.is_empty() && !index.contains('*') {
        return no_such_index(&index);
    }
    let mut per = serde_json::Map::new();
    for n in targets {
        if let Some(st) = store.get(&n) {
            st.write().closed = true;
            per.insert(n.clone(), json!({"closed": true}));
        }
    }
    respond(&p, json!({"acknowledged": true, "shards_acknowledged": true, "indices": per}))
}

pub async fn open_index(
    State(store): State<Store>,
    Path(index): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let targets = store.resolve(&index);
    if targets.is_empty() && !index.contains('*') {
        return no_such_index(&index);
    }
    for n in &targets {
        if let Some(st) = store.get(n) {
            st.write().closed = false;
        }
    }
    // `wait_for_completion=false` asks for the work to be tracked rather than
    // waited on; the index is already open, so the task is a finished one
    if p.get("wait_for_completion").map(|v| v == "false").unwrap_or(false) {
        return respond(&p, json!({"task": format!("node-0:open indices [{index}]")}));
    }
    respond(&p, json!({"acknowledged": true, "shards_acknowledged": true}))
}

pub async fn shards_ok(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"_shards": {"total": 1, "successful": 1, "failed": 0}}))
}
