//! `_plugins/_ml` -- the machine-learning registry, which here is empty.
//!
//! Nothing trains, deploys or runs a model in this engine, so every counter
//! below is zero because nothing has happened, and the registries are empty
//! because they hold nothing. The counts of models and connectors are read
//! from the indices the plugin keeps them in, so a caller who puts something
//! there is counted rather than argued with.

use super::*;

const MODELS: &str = ".plugins-ml-model";
const TASKS: &str = ".plugins-ml-task";
const CONNECTORS: &str = ".plugins-ml-connector";
const CONFIG: &str = ".plugins-ml-config";
const CONTROLLERS: &str = ".plugins-ml-controller";

fn stats_of(store: &Store) -> Stats {
    let mut stats = Stats::new(
        vec![
            ("ml_model_count", json!(backing_docs(store, MODELS).len())),
            ("ml_connector_count", json!(backing_docs(store, CONNECTORS).len())),
            ("ml_model_index_status", backing_index_status(store, MODELS)),
            ("ml_task_index_status", backing_index_status(store, TASKS)),
            ("ml_connector_index_status", backing_index_status(store, CONNECTORS)),
            ("ml_config_index_status", backing_index_status(store, CONFIG)),
            ("ml_controller_index_status", backing_index_status(store, CONTROLLERS)),
        ],
        vec![
            ("ml_jvm_heap_usage", json!(heap_usage_percent())),
            ("ml_request_count", json!(0)),
            ("ml_failure_count", json!(0)),
            ("ml_circuit_breaker_trigger_count", json!(0)),
            ("ml_executing_task_count", json!(0)),
            ("ml_deployed_model_count", json!(0)),
        ],
    );
    // the per-algorithm and per-model breakdowns travel with any node figure
    // the path asked for, and are empty for the same reason the registry is
    stats.always = vec![("algorithms", json!({})), ("models", json!({}))];
    // a cluster figure asked for on its own is answered on its own here: this
    // plugin does not name the node that was asked
    stats.empty_node_entry = false;
    stats
}

/// `GET _plugins/_ml/stats`, with a stat or a node in the path where one was
/// named. An unknown node is ignored, as the plugin ignores it -- the answer
/// is this node's either way.
pub async fn stats(State(store): State<Store>, Query(p): Query<Params>, uri: Uri) -> Response {
    let stats = stats_of(&store);
    let want = match stats.wanted(uri.path(), stat_in_path(&uri)) {
        Ok(want) => want,
        Err(refusal) => return refusal,
    };
    respond(&p, stats.answer(crate::cluster::identity().id.as_str(), want.as_deref()))
}

/// `GET _plugins/_ml/profile`, and the model and task views of it.
///
/// The profile describes the models a node has deployed and the tasks it is
/// running. Nothing is deployed and nothing runs, and the plugin answers an
/// empty object for a node with neither.
pub async fn profile(Query(p): Query<Params>) -> Response {
    respond(&p, json!({}))
}

/// `GET _plugins/_ml/memory` -- the conversations held for an agent. There is
/// no agent, so there are none.
pub async fn memory(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"memories": []}))
}

/// `GET _plugins/_ml/context_management` -- the agent templates registered on
/// the cluster. None is, and none can be until something runs them.
pub async fn context_management(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"total": 0, "templates": []}))
}

/// `GET _plugins/_ml/tools` -- the tools an agent may be given.
///
/// The reference lists the tool implementations its plugin ships. This engine
/// ships none, and saying so is the difference between a client finding no
/// tool and a client being told a tool exists that would never answer.
pub async fn tools(Query(p): Query<Params>) -> Response {
    respond(&p, json!([]))
}
