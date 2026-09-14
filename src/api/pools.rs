//! The thread pools a request passes through, as `_nodes/stats` and
//! `_cat/thread_pool` report them.
//!
//! OpenSearch hands each request to a pool named for its kind of work --
//! `search`, `write`, `get` -- and counts what each pool is running, has
//! queued, has refused and has finished. A request here runs on the async
//! runtime's workers rather than in a pool of its own, so those columns read
//! 0 however busy the node was, and a zero there was taken as evidence that
//! nothing was waiting. Every request is now counted under the pool
//! OpenSearch would have given it: `active` is how many of its kind are
//! running at this moment, `completed` how many have finished, `rejected` how
//! many were answered 429. `queue` is the runtime's own backlog -- the tasks
//! ready to run that no worker has picked up -- reported for `generic`, which
//! is where every other request waits too; the named pools keep no queue of
//! their own, and theirs is 0 because it is, not because it is unmeasured.

use axum::extract::Request;
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// One pool: how it is sized, and what it has done.
pub struct Pool {
    pub name: &'static str,
    /// `fixed` or `scaling`, as the reference sizes it
    pub kind: &'static str,
    active: AtomicU64,
    largest: AtomicU64,
    completed: AtomicU64,
    rejected: AtomicU64,
}

impl Pool {
    const fn new(name: &'static str, kind: &'static str) -> Pool {
        Pool {
            name,
            kind,
            active: AtomicU64::new(0),
            largest: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        }
    }

    pub fn active(&self) -> u64 {
        self.active.load(Relaxed)
    }

    pub fn largest(&self) -> u64 {
        self.largest.load(Relaxed)
    }

    pub fn completed(&self) -> u64 {
        self.completed.load(Relaxed)
    }

    pub fn rejected(&self) -> u64 {
        self.rejected.load(Relaxed)
    }

    /// The backlog waiting for this pool: the runtime's, for `generic`.
    pub fn queue(&self) -> u64 {
        if self.name != "generic" {
            return 0;
        }
        tokio::runtime::Handle::try_current()
            .map(|h| h.metrics().global_queue_depth() as u64)
            .unwrap_or(0)
    }

    /// How many threads the pool has: what a fixed pool is sized to, the
    /// runtime's workers for `generic`, and for a scaling pool the most it
    /// has had running at once.
    pub fn threads(&self) -> u64 {
        if self.name == "generic" {
            return tokio::runtime::Handle::try_current()
                .map(|h| h.metrics().num_workers() as u64)
                .unwrap_or(0)
                .max(self.largest());
        }
        match self.kind {
            "fixed" => self.size(),
            _ => self.largest(),
        }
    }

    /// The size the reference gives a fixed pool on a machine of this many
    /// processors; a scaling pool's largest.
    pub fn size(&self) -> u64 {
        let cpus = std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(1);
        match self.name {
            "search" => cpus * 3 / 2 + 1,
            "write" | "get" | "system_write" => cpus,
            "analyze" => 1,
            "force_merge" => 1,
            "search_throttled" => 1,
            "index_searcher" => cpus * 2,
            "listener" => (cpus / 2).clamp(1, 10),
            _ => self.largest().max(1),
        }
    }
}

/// Every pool, in name order, as the reference lists them.
pub static POOLS: [Pool; 16] = [
    Pool::new("analyze", "fixed"),
    Pool::new("fetch_shard_started", "scaling"),
    Pool::new("fetch_shard_store", "scaling"),
    Pool::new("flush", "scaling"),
    Pool::new("force_merge", "fixed"),
    Pool::new("generic", "scaling"),
    Pool::new("get", "fixed"),
    Pool::new("index_searcher", "fixed"),
    Pool::new("listener", "fixed"),
    Pool::new("management", "scaling"),
    Pool::new("refresh", "scaling"),
    Pool::new("search", "fixed"),
    Pool::new("search_throttled", "fixed"),
    Pool::new("snapshot", "scaling"),
    Pool::new("warmer", "scaling"),
    Pool::new("write", "fixed"),
];

fn pool(name: &str) -> &'static Pool {
    POOLS.iter().find(|p| p.name == name).unwrap_or(&POOLS[5])
}

/// The pool OpenSearch runs a request of this method and path in.
pub fn pool_for(method: &Method, path: &str) -> &'static str {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let has = |name: &str| parts.contains(&name);
    if has("_search") || has("_msearch") || has("_count") || has("_field_caps") || has("_explain") {
        return "search";
    }
    if has("_bulk") || has("_update") || has("_delete_by_query") || has("_update_by_query") {
        return "write";
    }
    if has("_doc") || has("_create") || has("_source") {
        return match *method {
            Method::GET | Method::HEAD => "get",
            _ => "write",
        };
    }
    if has("_mget") || has("_termvectors") || has("_mtermvectors") {
        return "get";
    }
    if has("_refresh") {
        return "refresh";
    }
    if has("_flush") {
        return "flush";
    }
    if has("_forcemerge") {
        return "force_merge";
    }
    if has("_analyze") {
        return "analyze";
    }
    if has("_snapshot") {
        return "snapshot";
    }
    if parts.first().map(|p| p.starts_with('_')).unwrap_or(true) {
        return "management";
    }
    "generic"
}

/// The layer that counts every request into its pool.
pub async fn track(request: Request, next: Next) -> Response {
    let p = pool(pool_for(request.method(), request.uri().path()));
    let now = p.active.fetch_add(1, Relaxed) + 1;
    p.largest.fetch_max(now, Relaxed);
    // a request whose client goes away is dropped mid-flight; the count it
    // took is given back either way
    struct Done(&'static Pool);
    impl Drop for Done {
        fn drop(&mut self) {
            self.0.active.fetch_sub(1, Relaxed);
            self.0.completed.fetch_add(1, Relaxed);
        }
    }
    let done = Done(p);
    let response = next.run(request).await;
    if response.status() == axum::http::StatusCode::TOO_MANY_REQUESTS {
        p.rejected.fetch_add(1, Relaxed);
    }
    drop(done);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_counted_under_the_pool_the_reference_runs_it_in() {
        assert_eq!(pool_for(&Method::POST, "/orders/_search"), "search");
        assert_eq!(pool_for(&Method::PUT, "/orders/_doc/1"), "write");
        assert_eq!(pool_for(&Method::GET, "/orders/_doc/1"), "get");
        assert_eq!(pool_for(&Method::POST, "/_bulk"), "write");
        assert_eq!(pool_for(&Method::GET, "/_cat/indices"), "management");
        assert_eq!(pool_for(&Method::PUT, "/orders"), "generic");
    }
}
