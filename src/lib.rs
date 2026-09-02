//! Library surface so benchmarks can drive the same code the server does.
// A handler answers with an axum `Response` either way, so the error half of
// its Result is as large as the ok half. Boxing it would put an allocation in
// front of every error a request can produce, to satisfy a lint about a shape
// that is deliberate.
#![allow(clippy::result_large_err)]
pub mod analysis;
pub mod api;
pub mod blockstats;
pub mod hdr;
pub mod painless;
pub mod query;
pub mod search;
pub mod snapshot;
pub mod source;
pub mod store;
pub mod tz;
