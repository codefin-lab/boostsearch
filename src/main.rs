//! boostsearch -- an OpenSearch-compatible search server on BoostCore.
//!
//! Conformance is driven by OpenSearch's own rest-api-spec YAML suite
//! (see tools/yaml_runner.py). Routes not yet ported answer 501.

mod api;
mod blockstats;
mod hdr;
mod search;
mod snapshot;
mod query;
mod source;
mod store;
mod tz;

use axum::Router;

/// Indexing allocates and frees heavily in bursts across many threads. glibc's
/// allocator holds those chunks rather than returning them, which reads as a
/// leak once there are hundreds of indices; mimalloc gives them back.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use axum::response::IntoResponse;
use axum::routing::{any, get, head, post, put};
use serde_json::json;
use store::Store;

async fn root() -> impl IntoResponse {
    axum::Json(json!({
        "name": "boostsearch",
        "cluster_name": "boostsearch",
        "cluster_uuid": "_na_",
        "version": {
            "distribution": "boostsearch",
            "number": "3.9.0",
            "lucene_version": "BoostCore-0.26",
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
        .route("/_boostsearch/memory", get(api::memory_report))
        // --- cluster ---
        .route("/_cluster/health", get(api::cluster_health))
        .route("/_cluster/health/{index}", get(api::cluster_health))
        .route("/_list/wlm_stats", get(api::wlm_stats_list))
        .route(
            "/_cluster/voting_config_exclusions",
            post(api::post_voting_config_exclusions).delete(api::delete_voting_config_exclusions),
        )
        .route("/_recovery", get(api::indices_recovery))
        .route("/{index}/_recovery", get(api::indices_recovery))
        .route("/_upgrade", post(api::indices_upgrade).get(api::indices_upgrade))
        .route("/{index}/_upgrade", post(api::indices_upgrade).get(api::indices_upgrade))
        .route(
            "/_cluster/allocation/explain",
            post(api::allocation_explain).get(api::allocation_explain),
        )
        .route("/{index}/_split/{target}", put(api::split_index).post(api::split_index))
        .route("/{index}/_shrink/{target}", put(api::shrink_index).post(api::shrink_index))
        .route("/{index}/_clone/{target}", put(api::clone_index).post(api::clone_index))
        .route("/{alias}/_rollover", post(api::rollover))
        .route("/{alias}/_rollover/{new_index}", post(api::rollover))
        .route("/_cluster/pending_tasks", get(api::pending_tasks))
        .route("/_search/point_in_time", post(api::create_pit).delete(api::delete_pit))
        .route("/{index}/_search/point_in_time", post(api::create_pit))
        .route("/_search/point_in_time/_all", get(api::get_all_pits).delete(api::delete_pit))
        .route("/_cluster/stats", get(api::cluster_stats))
        .route("/_cluster/stats/{*rest}", get(api::cluster_stats))
        .route("/_shard_stores", get(api::shard_stores))
        .route("/{index}/_shard_stores", get(api::shard_stores))
        .route("/_resolve/index/{name}", get(api::resolve_index))
        .route("/_remote/info", get(api::remote_info))
        .route("/{index}/_block/{block}", put(api::add_block).post(api::add_block))
        .route("/_mtermvectors", get(api::mtermvectors).post(api::mtermvectors))
        .route("/{index}/_mtermvectors", get(api::mtermvectors).post(api::mtermvectors))
        .route("/{index}/_termvectors/{id}", get(api::termvectors).post(api::termvectors))
        .route("/{index}/_termvectors", get(api::termvectors).post(api::termvectors))
        .route("/_cluster/state", get(api::cluster_state))
        .route("/_cluster/state/{*rest}", get(api::cluster_state_filtered))
        .route("/_cluster/settings", get(api::cluster_settings_get).put(api::cluster_settings_put))
        // --- aliases ---
        .route(
            "/_alias/{*rest}",
            get(api::get_alias_scoped)
                .head(api::exists_alias)
                .put(api::put_alias_named)
                .post(api::put_alias_named),
        )
        .route("/_aliases", post(api::update_aliases))
        // --- snapshots ---
        .route("/_snapshot", get(api::get_repository))
        .route(
            "/_snapshot/{repo}",
            put(api::put_repository)
                .post(api::put_repository)
                .get(api::get_repository)
                .delete(api::delete_repository),
        )
        .route("/_snapshot/{repo}/_verify", post(api::verify_repository))
        .route("/_snapshot/{repo}/_cleanup", post(api::cleanup_repository))
        .route("/_snapshot/_status", get(api::snapshot_status))
        .route(
            "/_snapshot/{repo}/{snapshot}",
            put(api::create_snapshot)
                .post(api::create_snapshot)
                .get(api::get_snapshot)
                .delete(api::delete_snapshot),
        )
        .route("/_snapshot/{repo}/{snapshot}/_status", get(api::snapshot_status))
        .route("/_snapshot/{repo}/{snapshot}/_restore", post(api::restore_snapshot))
        .route(
            "/_snapshot/{repo}/{snapshot}/_clone/{target}",
            put(api::clone_snapshot).post(api::clone_snapshot),
        )
        // --- pipelines ---
        .route("/_ingest/pipeline", get(api::get_ingest_pipeline))
        .route(
            "/_ingest/pipeline/{name}",
            put(api::put_ingest_pipeline)
                .get(api::get_ingest_pipeline)
                .delete(api::delete_ingest_pipeline),
        )
        .route("/_search/pipeline", get(api::get_search_pipeline))
        .route(
            "/_search/pipeline/{name}",
            put(api::put_search_pipeline)
                .get(api::get_search_pipeline)
                .delete(api::delete_search_pipeline),
        )
        // --- data streams ---
        .route("/_data_stream", get(api::get_data_stream))
        .route(
            "/_data_stream/{name}",
            put(api::create_data_stream)
                .post(api::create_data_stream)
                .get(api::get_data_stream)
                .delete(api::delete_data_stream),
        )
        // an index left out of the path leaves an empty segment behind, and
        // the body is expected to name it instead
        .route("//_alias/{name}", put(api::put_alias_named).post(api::put_alias_named))
        .route("//_alias/", put(api::put_alias_body).post(api::put_alias_body))
        .route("/{index}/_alias/{name}", put(api::put_alias).post(api::put_alias).delete(api::delete_alias))
        .route("/{index}/_aliases/{name}", put(api::put_alias).delete(api::delete_alias))
        .route("/{index}/_alias", put(api::put_alias_on_index))
        .route("/{index}/_aliases", put(api::put_alias_on_index))
        .route("/_alias", put(api::put_alias_body))
        // an alias left out of the path leaves the trailing slash behind
        .route("/_alias/", put(api::put_alias_body).post(api::put_alias_body))
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
               put(api::put_component_template).get(api::get_component_template)
                   .delete(api::delete_component_template))
        .route("/_component_template", get(api::get_component_template))
        .route("/_index_template/_simulate",
               post(api::simulate_template).put(api::simulate_template))
        .route("/_index_template/_simulate/{name}",
               post(api::simulate_template).put(api::simulate_template))
        .route("/_index_template/_simulate_index/{index}",
               post(api::simulate_index_template).put(api::simulate_index_template))
        // --- nodes and cluster housekeeping ---
        .route("/_nodes/stats", get(api::nodes_stats))
        .route("/_nodes/stats/{*rest}", get(api::nodes_stats))
        .route("/_nodes", get(api::nodes_info))
        .route("/_nodes/{*rest}", get(api::nodes_info))
        .route("/_cluster/reroute", post(api::reroute))
        .route("/_script_context", get(api::script_contexts))
        .route("/_script_language", get(api::script_languages))
        .route("/_tasks/_cancel", post(api::cancel_tasks))
        .route("/_tasks/{id}/_cancel", post(api::cancel_tasks))
        .route("/_tasks", get(api::list_tasks))
        .route("/_tasks/{id}", get(api::get_task))
        // --- index housekeeping ---
        .route("/_cat/segments", get(api::cat_segments))
        .route("/_cat/segments/{index}", get(api::cat_segments))
        .route("/_segments", get(api::segments))
        .route("/{index}/_segments", get(api::segments))
        .route("/_flush", post(api::flush).get(api::flush))
        .route("/{index}/_flush", post(api::flush).get(api::flush))
        .route("/_cache/clear", post(api::shards_ok))
        .route("/{index}/_cache/clear", post(api::shards_ok))
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
        .route("/_cat/allocation", get(api::cat_allocation))
        .route("/_cat/allocation/{node}", get(api::cat_allocation))
        .route("/_cat/nodeattrs", get(api::cat_nodeattrs))
        .route("/_cat/plugins", get(api::cat_plugins))
        .route("/_cat/thread_pool", get(api::cat_thread_pool))
        .route("/_cat/thread_pool/{patterns}", get(api::cat_thread_pool))
        .route("/_cat/tasks", get(api::cat_tasks))
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
        // A bulk request is as big as the client wants to make it. Axum
        // stops at 2 MB by default, which is smaller than any bulk helper's
        // idea of a batch; OpenSearch's own ceiling is 100 MB, so that is the
        // one to keep. `BOOSTSEARCH_MAX_CONTENT_MB` moves it.
        .layer(axum::extract::DefaultBodyLimit::max(max_content_bytes()))
        .with_state(store)
}

/// How large a request body may be, in bytes.
fn max_content_bytes() -> usize {
    std::env::var("BOOSTSEARCH_MAX_CONTENT_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|mb| *mb > 0)
        .unwrap_or(100)
        * 1024
        * 1024
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::WARN).init();
    let addr = std::env::var("BOOSTSEARCH_ADDR").unwrap_or_else(|_| "127.0.0.1:9200".into());
    // BOOSTSEARCH_DATA=<dir> keeps indices on disk (mmapped, and they survive a
    // restart); unset keeps everything in RAM, which is what the test suite wants.
    let store = match std::env::var("BOOSTSEARCH_DATA") {
        Ok(dir) if !dir.is_empty() => Store::on_disk(&dir)?,
        _ => Store::new(),
    };
    // anything acknowledged but not committed when the process last stopped is
    // in a translog and nowhere else; it goes back into the index before the
    // first request is answered
    api::recover(&store);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("boostsearch listening on {addr}");
    axum::serve(listener, app(store)).await?;
    Ok(())
}
