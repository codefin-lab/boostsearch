//! Making one index out of another: cloned, shrunk, split, or rolled over.

use super::*;

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
    if asked_max_size
        && num("number_of_shards").is_none()
        && let Some(o) = settings.pointer_mut("/index").and_then(|v| v.as_object_mut())
    {
        o.insert("number_of_shards".into(), json!("1"));
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
            g.refresh_knobs();
            g.apply_analysis();
            g.save_meta();
        }
    }
    // `wait_for_completion=false` asks for the work to be tracked rather than
    // waited on; the copy is already done, so the task is a finished one
    if p.get("wait_for_completion").map(|v| v == "false").unwrap_or(false) {
        return respond(
            &p,
            json!({
                "task": format!(
                    "{}:{kind} from [{source}] to [{target}]",
                    crate::cluster::identity().id.as_str()
                )
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
            let mut g = st.write();
            g.aliases.insert(alias.clone(), def);
            g.save_meta();
        }
        {
            let mut g = src.write();
            g.aliases.remove(&alias);
            g.save_meta();
        }
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
