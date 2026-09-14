//! An index's settings, and the defaults underneath them.

use super::*;

/// Settings OpenSearch reports under `defaults` when asked for them. Only the
/// handful the conformance suite reads are modelled.
pub(crate) fn default_settings() -> Value {
    json!({"index": {
        "refresh_interval": "1s",
        "max_result_window": "10000",
        "number_of_routing_shards": "1",
        "codec": "default",
        "auto_expand_replicas": "false",
        "max_inner_result_window": "100",
        "max_rescore_window": "10000",
        "query": {"default_field": ["*"]},
    }})
}

/// `flat_settings=true` renders `{"index":{"a":1}}` as `{"index.a":1}`.
pub(crate) fn flatten_settings(v: &Value, prefix: &str, out: &mut serde_json::Map<String, Value>) {
    match v {
        Value::Object(o) => {
            for (k, child) in o {
                let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten_settings(child, &path, out);
            }
        }
        leaf => {
            out.insert(prefix.to_string(), leaf.clone());
        }
    }
}

/// Take a setting out of every spelling it may have been written in: under
/// `index` or at the top, flat or nested, with its `index.` prefix or without.
fn forget_setting(settings: &mut Value, key: &str) {
    fn remove_path(node: &mut Value, parts: &[&str]) {
        let Some(o) = node.as_object_mut() else { return };
        // the rest of the path written flat under this object
        o.remove(&parts.join("."));
        if parts.len() > 1
            && let Some(child) = o.get_mut(parts[0])
        {
            remove_path(child, &parts[1..]);
        }
    }
    let parts: Vec<&str> = key.split('.').collect();
    let prefixed: Vec<&str> = std::iter::once("index").chain(parts.iter().copied()).collect();
    remove_path(settings, &parts);
    remove_path(settings, &prefixed);
    if let Some(index) = settings.get_mut("index") {
        remove_path(index, &parts);
        remove_path(index, &prefixed);
    }
}

/// `human` asks for a readable form beside each machine one.
pub(crate) fn add_human_settings(view: &mut Value, st: &IdxState) {
    let created = st.created_millis();
    let text = st.created_string();
    if let Some(o) = view.pointer_mut("/index").and_then(|v| v.as_object_mut()) {
        o.insert("creation_date_string".into(), json!(text));
        o.entry("creation_date".to_string()).or_insert_with(|| json!(created.to_string()));
        // the version an index was made under is reported the same two ways
        let ver = o
            .get("version")
            .and_then(|v| v.get("created"))
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "136407827".to_string());
        o.insert(
            "version".into(),
            json!({
                "created": ver, "created_string": "3.9.0",
            }),
        );
    } else if let Some(o) = view.as_object_mut() {
        o.insert("index.creation_date_string".into(), json!(text));
        o.entry("index.creation_date".to_string()).or_insert_with(|| json!(created.to_string()));
    }
}

pub(crate) fn settings_view(raw: &Value, name: Option<&str>, flat: bool) -> Value {
    let mut flat_map = serde_json::Map::new();
    flatten_settings(raw, "", &mut flat_map);
    // how many routing shards an index was made with is kept for routing,
    // not as a setting anyone set, and the reference does not list it
    flat_map.remove("index.boost_routing_shards");
    if let Some(name) = name
        && name != "_all"
        && name != "*"
    {
        let pats: Vec<regex::Regex> =
            name.split(',').map(|p| crate::store::wildcard_to_regex(p.trim())).collect();
        flat_map.retain(|k, _| pats.iter().any(|re| re.is_match(k)));
    }
    if flat {
        return Value::Object(flat_map);
    }
    // rebuild the nested shape from whatever survived the filter
    let mut nested = json!({});
    for (k, v) in flat_map {
        let mut cur = &mut nested;
        let segs: Vec<&str> = k.split('.').collect();
        for seg in &segs[..segs.len() - 1] {
            cur = entry_of(cur, seg, || json!({}));
        }
        if let Some(o) = cur.as_object_mut() {
            o.insert(segs[segs.len() - 1].to_string(), v);
        }
    }
    nested
}

pub async fn get_settings(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    settings_response(store, index.map(|Path(i)| i), None, p)
}

pub async fn get_settings_all_named(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    settings_response(store, None, Some(name), p)
}

pub async fn get_settings_named(
    State(store): State<Store>,
    Path((index, name)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    settings_response(store, Some(index), Some(name), p)
}

pub(crate) fn settings_response(
    store: Store,
    index: Option<String>,
    name: Option<String>,
    p: Params,
) -> Response {
    let expr = index.unwrap_or_else(|| "_all".into());
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !ignore_unavailable(&p) {
        return no_such_index(&expr);
    }
    let flat = flag(&p, "flat_settings");
    let mut out = serde_json::Map::new();
    for n in targets {
        let Some(st) = store.get(&n) else { continue };
        let raw = st.read().effective_settings();
        let mut entry = json!({ "settings": settings_view(&raw, name.as_deref(), flat) });
        if flag(&p, "human") {
            add_human_settings(&mut entry["settings"], &st.read());
        }
        if flag(&p, "include_defaults") {
            entry["defaults"] = settings_view(&default_settings(), name.as_deref(), flat);
        }
        out.insert(n.clone(), entry);
    }
    // through `respond`, like every other answer: returned as bare JSON, it
    // skipped the one place `filter_path` is applied, and
    // `?filter_path=**.refresh_interval` answered with every setting
    respond(&p, Value::Object(out))
}

pub async fn put_settings(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let expr = index.map(|Path(i)| i).unwrap_or_else(|| "_all".into());
    let targets = store.resolve(&expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" && !ignore_unavailable(&p) {
        return no_such_index(&expr);
    }
    // a settings body may arrive wrapped in `settings`, wrapped in `index`, or flat
    let patch = body.get("settings").unwrap_or(&body);
    let patch = patch.get("index").unwrap_or(patch).clone();
    // `preserve_existing` says to fill in only what is not already set
    let preserve = p.get("preserve_existing").map(|v| v != "false").unwrap_or(false);
    // how an open index places its documents is fixed when it is made: a
    // shard count or a routing fold changed under it would move every
    // document it holds to a shard it is not on
    let mut named = serde_json::Map::new();
    flatten_settings(&patch, "", &mut named);
    let fixed: Vec<String> = named
        .keys()
        .map(|k| k.strip_prefix("index.").unwrap_or(k).to_string())
        .filter(|k| {
            matches!(
                k.as_str(),
                "number_of_shards" | "number_of_routing_shards" | "routing_partition_size"
            )
        })
        .map(|k| format!("index.{k}"))
        .collect();
    if !fixed.is_empty() {
        let open: Vec<String> = targets
            .iter()
            .filter_map(|n| store.get(n))
            .filter(|st| !st.read().closed)
            .map(|st| {
                let g = st.read();
                format!("{}/{}", g.name, g.uuid)
            })
            .collect();
        if !open.is_empty() {
            return err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!(
                    "Can't update non dynamic settings [[{}]] for open indices [[{}]]",
                    fixed.join(", "),
                    open.join(", ")
                ),
            );
        }
    }
    for n in targets {
        let Some(st) = store.get(&n) else { continue };
        let mut g = st.write();
        let mut settings = g.settings.clone();
        if !settings.is_object() {
            settings = json!({});
        }
        let mut patch = patch.clone();
        if preserve && let Some(o) = patch.as_object_mut() {
            // a key may be written with the `index.` prefix the setting
            // lookup adds for itself
            o.retain(|k, _| g.setting(k.strip_prefix("index.").unwrap_or(k)).is_none());
        }
        let slot = entry_of(&mut settings, "index", || json!({}));
        crate::store::deep_merge(slot, &patch);
        // A null resets a setting to its default. Merged in, it only covered
        // one of the places a setting can be written, and the value the index
        // was created with -- kept outside `index`, or spelled flat -- showed
        // through again: `max_result_window` made 20, set to 10000 and reset
        // read 20 where the reference reads the default.
        let mut flat = serde_json::Map::new();
        flatten_settings(&patch, "", &mut flat);
        for (key, value) in flat {
            if value.is_null() {
                forget_setting(&mut settings, key.strip_prefix("index.").unwrap_or(&key));
            }
        }
        g.settings = settings;
        g.refresh_knobs();
        g.apply_analysis();
        g.save_meta();
    }
    respond(&p, json!({"acknowledged": true}))
}
