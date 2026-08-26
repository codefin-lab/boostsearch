//! obsearch -- an OpenSearch-compatible search server on tantivy.
//!
//! Conformance is driven by OpenSearch's own rest-api-spec YAML suite
//! (see tools/yaml_runner.py). Routes not yet ported answer 501.

mod api;
mod blockstats;
mod search;
mod query;
mod source;
mod store;

use axum::Router;
use axum::response::IntoResponse;
use axum::routing::{any, delete, get, head, post, put};
use serde_json::json;
use store::Store;

async fn root() -> impl IntoResponse {
    axum::Json(json!({
        "name": "obsearch",
        "cluster_name": "obsearch",
        "version": {
            "distribution": "obsearch",
            "number": "3.9.0",
            "lucene_version": "tantivy-0.26",
        },
        "tagline": "You Know, for Search"
    }))
}

fn app(store: Store) -> Router {
    Router::new()
        .route("/", any(root))
        // --- bulk (static paths must be declared before `/{index}`) ---
        .route("/_bulk", post(api::bulk).put(api::bulk))
        .route("/{index}/_bulk", post(api::bulk).put(api::bulk))
        // --- search ---
        .route("/_search", get(api::search).post(api::search))
        .route("/{index}/_search", get(api::search).post(api::search))
        .route("/_count", get(api::count).post(api::count))
        .route("/{index}/_count", get(api::count).post(api::count))
        .route("/_msearch", get(api::msearch).post(api::msearch))
        .route("/{index}/_msearch", get(api::msearch).post(api::msearch))
        .route("/_mget", get(api::mget).post(api::mget))
        .route("/{index}/_mget", get(api::mget).post(api::mget))
        .route("/{index}/_update/{id}", post(api::update_doc))
        .route("/_forcemerge", post(api::force_merge))
        .route("/{index}/_forcemerge", post(api::force_merge))
        .route("/_stats", get(api::stats))
        .route("/{index}/_stats", get(api::stats))
        .route("/_stats/{metric}", get(api::stats_metric))
        .route("/{index}/_stats/{metric}", get(api::stats_index_metric))
        .route("/{index}/_explain/{id}", get(api::explain).post(api::explain))
        .route("/_field_caps", get(api::field_caps).post(api::field_caps))
        .route("/{index}/_field_caps", get(api::field_caps).post(api::field_caps))
        .route("/_alias", get(api::get_alias))
        // --- refresh ---
        .route("/_refresh", post(api::refresh_all).get(api::refresh_all))
        .route("/{index}/_refresh", post(api::refresh_index).get(api::refresh_index))
        // --- mappings / settings ---
        .route("/_mapping", get(api::get_mapping))
        .route("/{index}/_mapping", get(api::get_mapping).put(api::put_mapping))
        .route("/_settings", get(api::get_settings))
        .route("/_settings/{name}", get(api::get_settings_all_named))
        .route("/{index}/_settings", get(api::get_settings))
        .route("/{index}/_settings/{name}", get(api::get_settings_named))
        // --- documents ---
        .route(
            "/{index}/_doc/{id}",
            put(api::index_doc).post(api::index_doc).get(api::get_doc).delete(api::delete_doc_route),
        )
        .route("/{index}/_doc/{id}", head(api::head_doc))
        .route("/{index}/_doc", post(api::index_doc_auto))
        .route("/{index}/_create/{id}", put(api::create_doc).post(api::create_doc))
        .route("/{index}/_source/{id}", get(api::get_source).head(api::head_doc))
        // --- index lifecycle ---
        .route(
            "/{index}",
            put(api::create_index)
                .delete(api::delete_index)
                .head(api::index_exists)
                .post(api::create_index),
        )
        .fallback(any(api::not_ported))
        // a path we route but with an unported method should read as "not ported",
        // not as a 405 the suite cannot interpret
        .method_not_allowed_fallback(api::not_ported)
        .with_state(store)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::WARN).init();
    let addr = std::env::var("OBSEARCH_ADDR").unwrap_or_else(|_| "127.0.0.1:9200".into());
    // OBSEARCH_DATA=<dir> keeps indices on disk (mmapped, and they survive a
    // restart); unset keeps everything in RAM, which is what the test suite wants.
    let store = match std::env::var("OBSEARCH_DATA") {
        Ok(dir) if !dir.is_empty() => Store::on_disk(&dir)?,
        _ => Store::new(),
    };
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("obsearch listening on {addr}");
    axum::serve(listener, app(store)).await?;
    Ok(())
}
