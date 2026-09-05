//! What a search already answered, kept for the next caller who asks it.
//!
//! A dashboard is a handful of aggregations asked over and over by everyone
//! looking at it, over an index that is not being written to between one look
//! and the next. OpenSearch keeps those answers per shard and serves them
//! again without walking the index; this is the same cache, with the same
//! rules about what may go in it.
//!
//! Only a search that asks for no documents is cached -- `size: 0`, which in
//! practice means aggregations -- because a request that returns hits is
//! rarely asked twice with the same answer expected. An entry is thrown away
//! the moment anything about the index it came from changes: the key carries
//! a number that every write, refresh and mapping change moves on, so a stale
//! answer cannot be found rather than being found and checked.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use parking_lot::{Mutex, RwLock};
use serde_json::Value;

use crate::store::Store;

/// How much of the process the cache may hold. OpenSearch's default is one
/// percent of the heap; this is a fixed ceiling instead, because there is no
/// heap to take a percentage of.
const MAX_BYTES: usize = 64 * 1024 * 1024;
/// A ceiling on entries as well as bytes: many small answers cost lookups
/// even when they cost little memory.
const MAX_ENTRIES: usize = 20_000;

#[derive(Default)]
pub struct RequestCache {
    entries: RwLock<HashMap<String, (Value, usize)>>,
    /// keys oldest first, so the oldest is what leaves when room is needed
    order: Mutex<std::collections::VecDeque<String>>,
    bytes: AtomicUsize,
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub evictions: AtomicU64,
}

impl RequestCache {
    pub fn get(&self, key: &str) -> Option<Value> {
        match self.entries.read().get(key) {
            Some((v, _)) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(v.clone())
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    pub fn put(&self, key: String, value: Value) {
        let size = key.len() + value.to_string().len();
        // one answer is never worth a tenth of the whole cache
        if size > MAX_BYTES / 10 {
            return;
        }
        {
            let mut entries = self.entries.write();
            if entries.contains_key(&key) {
                return;
            }
            entries.insert(key.clone(), (value, size));
        }
        self.bytes.fetch_add(size, Ordering::Relaxed);
        self.order.lock().push_back(key);
        while self.bytes.load(Ordering::Relaxed) > MAX_BYTES
            || self.entries.read().len() > MAX_ENTRIES
        {
            let Some(oldest) = self.order.lock().pop_front() else { break };
            if let Some((_, size)) = self.entries.write().remove(&oldest) {
                self.bytes.fetch_sub(size, Ordering::Relaxed);
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// `_cache/clear?request=true`, and anything else that empties it.
    pub fn clear(&self) {
        self.entries.write().clear();
        self.order.lock().clear();
        self.bytes.store(0, Ordering::Relaxed);
    }

    /// Only the entries an index put there, for a clear naming one index.
    pub fn clear_index(&self, name: &str) {
        let prefix = format!("{name}\u{1}");
        let mut entries = self.entries.write();
        let gone: Vec<String> = entries
            .keys()
            .filter(|k| k.split('\u{2}').any(|part| part.starts_with(&prefix)))
            .cloned()
            .collect();
        for k in gone {
            if let Some((_, size)) = entries.remove(&k) {
                self.bytes.fetch_sub(size, Ordering::Relaxed);
            }
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }
}

/// Whether this search may be answered from the cache, and remembered in it.
///
/// A request that asks for documents is not cached: only one that asks what
/// the index contains in aggregate. Anything whose answer depends on when it
/// was asked -- `now` in a range, a script that reads the clock -- is not
/// cached either, since the same key would stand for two different answers.
pub fn cacheable(store: &Store, targets: &[String], body: &Value, p: &crate::api::Params) -> bool {
    if p.contains_key("scroll") || p.contains_key("preference") {
        return false;
    }
    // `pre_filter_shard_size` asks which shards could be skipped, and the
    // answer says how many were: a remembered one would report the skipping
    // that a previous request did rather than this one
    if p.contains_key("pre_filter_shard_size")
        || body.get("pre_filter_shard_size").is_some()
    {
        return false;
    }
    if p.get("request_cache").map(|v| v == "false").unwrap_or(false) {
        return false;
    }
    let size = body
        .get("size")
        .and_then(|v| v.as_i64())
        .or_else(|| p.get("size").and_then(|v| v.parse().ok()));
    // `size` unstated means ten hits, which is not a cacheable shape
    if size != Some(0) {
        return false;
    }
    if body.get("profile").and_then(|v| v.as_bool()).unwrap_or(false) {
        return false;
    }
    // an index may say it does not want its requests cached
    for name in targets {
        let Some(st) = store.get(name) else { return false };
        let g = st.read();
        if g.setting("requests.cache.enable").map(|v| v == "false").unwrap_or(false) {
            return false;
        }
    }
    // `now` reads the clock, so the same request is a different question a
    // minute later; OpenSearch refuses to cache these for the same reason
    !body.to_string().contains("now")
}

/// What names this exact question, over exactly this state of the index.
///
/// The generation numbers are what make an entry go stale: every write and
/// every refresh moves the index's number on, so an answer from before it can
/// never be found again.
pub fn key(store: &Store, expr: &str, targets: &[String], body: &Value, p: &crate::api::Params) -> String {
    let mut parts = vec![format!("{expr}\u{1}")];
    for name in targets {
        let generation = store.get(name).map(|st| st.read().generation()).unwrap_or(0);
        parts.push(format!("{name}\u{1}{generation}"));
    }
    // two callers may be allowed to see different documents of the same
    // index, so an answer is only ever handed back to the caller it was
    // worked out for
    let who = crate::security::layer::current_caller()
        .map(|c| format!("{}:{:?}", c.name, c.roles))
        .unwrap_or_default();
    parts.push(who);
    // the parameters that change an answer rather than how it is printed
    let mut params: Vec<String> = p
        .iter()
        .filter(|(k, _)| {
            matches!(
                k.as_str(),
                "routing"
                    | "q"
                    | "df"
                    | "analyzer"
                    | "default_operator"
                    | "search_type"
                    | "terminate_after"
                    | "min_score"
                    | "expand_wildcards"
                    | "ignore_unavailable"
                    | "allow_no_indices"
            )
        })
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    params.sort();
    parts.push(params.join("&"));
    parts.push(body.to_string());
    parts.join("\u{2}")
}
