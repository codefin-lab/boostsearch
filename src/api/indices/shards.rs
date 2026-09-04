//! What an index is made of, and what state those parts are in.

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
        g.refresh_knobs();
        g.apply_analysis();
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
    method: axum::http::Method,
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
    // asking is not the same as doing: a GET reports how much of each index
    // would have to be rewritten, a POST reports what the rewriting did
    if method == axum::http::Method::GET {
        let mut indices = serde_json::Map::new();
        let mut total = 0u64;
        for n in &names {
            let bytes = store.index_size(n);
            total += bytes;
            indices.insert(
                n.clone(),
                json!({
                    "size_in_bytes": bytes,
                    "size_to_upgrade_in_bytes": 0,
                    "size_to_upgrade_ancient_in_bytes": 0,
                }),
            );
        }
        return respond(
            &p,
            json!({
                "size_in_bytes": total,
                "size_to_upgrade_in_bytes": 0,
                "size_to_upgrade_ancient_in_bytes": 0,
                "indices": Value::Object(indices),
            }),
        );
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

pub async fn shards_ok(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"_shards": {"total": 1, "successful": 1, "failed": 0}}))
}
