//! What an index has been asked to do, counted as it happens.
//!
//! `_stats` reported the live document count as `indexing.index_total`, and
//! zero for deletes, refreshes, merges and the time any of it took: a document
//! written three times read as one write, and a dashboard graphing the rate of
//! writes graphed the size of the index. These are the counts OpenSearch
//! keeps per shard, kept per index here -- an index is its shards on this
//! node -- and reset, as there, when the node starts.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// One kind of operation: how many finished, how long they took together, and
/// how many are running now.
#[derive(Default)]
pub struct Tally {
    total: AtomicU64,
    nanos: AtomicU64,
    current: AtomicU64,
}

impl Tally {
    /// An operation begins; it is counted when the guard is dropped.
    pub fn start(&self) -> Running<'_> {
        self.current.fetch_add(1, Relaxed);
        Running { tally: self, started: std::time::Instant::now() }
    }

    /// Operations begun (a positive `delta`) or ended (a negative one), for
    /// a caller that counts the time itself.
    pub fn current_add(&self, delta: i64) {
        match delta >= 0 {
            true => self.current.fetch_add(delta as u64, Relaxed),
            false => self.current.fetch_sub(delta.unsigned_abs(), Relaxed),
        };
    }

    /// An operation that took `nanos`, counted after the fact.
    pub fn add(&self, nanos: u64) {
        self.total.fetch_add(1, Relaxed);
        self.nanos.fetch_add(nanos, Relaxed);
    }

    pub fn total(&self) -> u64 {
        self.total.load(Relaxed)
    }

    pub fn millis(&self) -> u64 {
        self.nanos.load(Relaxed) / 1_000_000
    }

    pub fn current(&self) -> u64 {
        self.current.load(Relaxed)
    }
}

/// An operation under way, counted into its tally when it ends.
pub struct Running<'a> {
    tally: &'a Tally,
    started: std::time::Instant,
}

impl Running<'_> {
    pub fn elapsed_nanos(&self) -> u64 {
        self.started.elapsed().as_nanos() as u64
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.tally.current.fetch_sub(1, Relaxed);
        self.tally.add(self.elapsed_nanos());
    }
}

/// The phases of a search, which `_stats` reports for the index and for each
/// group a search named with `stats`.
#[derive(Default)]
pub struct SearchTally {
    pub query: Tally,
    pub query_failed: AtomicU64,
    pub fetch: Tally,
    pub scroll: Tally,
    pub suggest: Tally,
}

#[derive(Default)]
pub struct Counters {
    pub index: Tally,
    pub index_failed: AtomicU64,
    /// when a write last reached the index, in milliseconds since the epoch
    pub last_index_ms: AtomicU64,
    pub delete: Tally,
    pub get: Tally,
    pub get_exists: Tally,
    pub get_missing: Tally,
    pub search: SearchTally,
    /// the same phases again for each group a search named
    pub groups: RwLock<HashMap<String, Arc<SearchTally>>>,
    /// every refresh, whoever asked for it
    pub refresh: Tally,
    /// the refreshes a caller asked for: `_refresh`, and `refresh=true`
    pub refresh_external: Tally,
    pub flush: Tally,
    pub merge: Tally,
    pub merge_docs: AtomicU64,
    pub merge_bytes: AtomicU64,
}

impl Counters {
    /// The tally of one search group, made the first time it is named.
    pub fn group(&self, name: &str) -> Arc<SearchTally> {
        if let Some(t) = self.groups.read().get(name) {
            return t.clone();
        }
        self.groups.write().entry(name.to_string()).or_default().clone()
    }
}
