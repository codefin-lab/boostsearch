//! obsearch -- an OpenSearch-compatible search server on tantivy.
//!
//! Conformance is driven by OpenSearch's own rest-api-spec YAML suite
//! (see tools/yaml_runner.py). Routes not yet ported answer 501.

mod api;
mod blockstats;
mod hdr;
mod search;
mod query;
mod source;
mod store;

use axum::Router;

/// Indexing allocates and frees heavily in bursts across many threads. glibc's
/// allocator holds those chunks rather than returning them, which reads as a
/// leak once there are hundreds of indices; mimalloc gives them back.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
        .route("/_search/scroll", get(api::scroll).post(api::scroll).delete(api::clear_scroll))
        .route("/_search/scroll/{id}", get(api::scroll).post(api::scroll).delete(api::clear_scroll))
        .route("/_mapping/field/{fields}", get(api::get_field_mapping))
        .route("/{index}/_mapping/field/{fields}", get(api::get_field_mapping))
        .route("/{index}/_search", get(api::search).post(api::search))
        .route("/_count", get(api::count).post(api::count))
        .route("/{index}/_count", get(api::count).post(api::count))
        .route("/_msearch", get(api::msearch).post(api::msearch))
        .route("/{index}/_msearch", get(api::msearch).post(api::msearch))
        .route("/_mget", get(api::mget).post(api::mget))
        .route("/{index}/_mget", get(api::mget).post(api::mget))
        .route("/{index}/_update/{id}", post(api::update_doc))
        .route("/_obsearch/memory", get(api::memory_report))
        // --- cluster ---
        .route("/_cluster/health", get(api::cluster_health))
        .route("/_cluster/health/{index}", get(api::cluster_health))
        .route("/_cluster/state", get(api::cluster_state))
        .route("/_cluster/state/{*rest}", get(api::cluster_state))
        .route("/_cluster/settings", get(api::cluster_settings_get).put(api::cluster_settings_put))
        // --- aliases ---
        .route("/_alias/{*rest}", get(api::get_alias_scoped).head(api::exists_alias))
        .route("/_aliases", post(api::update_aliases))
        .route("/{index}/_alias/{name}", put(api::put_alias).post(api::put_alias).delete(api::delete_alias))
        .route("/{index}/_aliases/{name}", put(api::put_alias).delete(api::delete_alias))
        // --- templates ---
        .route("/_template/{name}",
               put(api::put_template).post(api::put_template)
                   .get(api::get_template).head(api::exists_template)
                   .delete(api::delete_template))
        .route("/_template", get(api::get_template))
        .route("/_index_template/{name}",
               put(api::put_index_template).post(api::put_index_template)
                   .get(api::get_index_template).delete(api::delete_index_template))
        .route("/_index_template", get(api::get_index_template))
        .route("/_component_template/{name}",
               put(api::put_index_template).get(api::get_index_template)
                   .delete(api::delete_index_template))
        // --- nodes and cluster housekeeping ---
        .route("/_nodes", get(api::nodes_info))
        .route("/_nodes/{*rest}", get(api::nodes_info))
        .route("/_cluster/reroute", post(api::acknowledged))
        .route("/_cluster/allocation/explain", get(api::acknowledged).post(api::acknowledged))
        .route("/_cluster/pending_tasks", get(api::acknowledged))
        // --- index housekeeping ---
        .route("/_flush", post(api::shards_ok).get(api::shards_ok))
        .route("/{index}/_flush", post(api::shards_ok).get(api::shards_ok))
        .route("/_cache/clear", post(api::shards_ok))
        .route("/{index}/_cache/clear", post(api::shards_ok))
        .route("/_upgrade", post(api::shards_ok).get(api::shards_ok))
        .route("/{index}/_upgrade", post(api::shards_ok).get(api::shards_ok))
        .route("/_recovery", get(api::acknowledged))
        .route("/{index}/_recovery", get(api::acknowledged))
        .route("/_search_shards", get(api::search_shards).post(api::search_shards))
        .route("/{index}/_search_shards", get(api::search_shards).post(api::search_shards))
        .route("/_validate/query", get(api::validate_query).post(api::validate_query))
        .route("/{index}/_validate/query", get(api::validate_query).post(api::validate_query))
        .route("/_analyze", get(api::analyze).post(api::analyze))
        .route("/{index}/_analyze", get(api::analyze).post(api::analyze))
        // --- index management ---
        .route("/{index}/_close", post(api::close_index))
        .route("/{index}/_open", post(api::open_index))
        .route("/_settings", put(api::put_settings))
        // --- cat ---
        .route("/_cat/{what}", get(api::cat_dispatch))
        .route("/_cat/{what}/{target}", get(api::cat_dispatch_target))
        .route("/_cat/indices", get(api::cat_indices))
        .route("/_cat/indices/{index}", get(api::cat_indices))
        .route("/_cat/aliases", get(api::cat_aliases))
        .route("/_cat/aliases/{name}", get(api::cat_aliases))
        .route("/_cat/count", get(api::cat_count))
        .route("/_cat/count/{index}", get(api::cat_count))
        .route("/_cat/health", get(api::cat_health))
        .route("/_forcemerge", post(api::force_merge))
        .route("/{index}/_forcemerge", post(api::force_merge))
        .route("/_stats", get(api::stats))
        .route("/{index}/_stats", get(api::stats))
        .route("/_stats/{metric}", get(api::stats_metric))
        .route("/{index}/_stats/{metric}", get(api::stats_index_metric))
        .route("/{index}/_explain/{id}", get(api::explain).post(api::explain))
        .route("/_field_caps", get(api::field_caps).post(api::field_caps))
        .route("/{index}/_field_caps", get(api::field_caps).post(api::field_caps))
        .route("/_alias", get(api::get_alias_scoped))
        // --- refresh ---
        .route("/_refresh", post(api::refresh_all).get(api::refresh_all))
        .route("/{index}/_refresh", post(api::refresh_index).get(api::refresh_index))
        // --- mappings / settings ---
        .route("/_mapping", get(api::get_mapping))
        .route("/{index}/_mapping", get(api::get_mapping).put(api::put_mapping))
        .route("/_settings", get(api::get_settings))
        .route("/_settings/{name}", get(api::get_settings_all_named))
        .route("/{index}/_settings", get(api::get_settings).put(api::put_settings))
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
        .route("/{index}/_alias", get(api::index_alias_list))
        .route("/{index}/_alias/{name}", get(api::index_alias_get).head(api::index_alias_head))
        .route(
            "/{index}",
            get(api::get_index)
                .put(api::create_index)
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
