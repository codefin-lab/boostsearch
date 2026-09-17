//! `_plugins/_anomaly_detection` and `_plugins/_forecast` -- the two halves of
//! the time-series plugin's read surface.
//!
//! No detector and no forecaster runs here, so nothing has been executed,
//! cancelled or failed and every counter says so. What is not fixed is how
//! many configurations there are: those are counted out of the indices the
//! plugin keeps them in, so a caller who writes a detector or a forecaster
//! there is counted rather than answered with a constant.

use super::*;

const DETECTORS: &str = ".opendistro-anomaly-detectors";
const RESULTS: &str = ".opendistro-anomaly-results";
const STATE: &str = ".opendistro-anomaly-detection-state";
const CHECKPOINTS: &str = ".opendistro-anomaly-checkpoints";
const JOBS: &str = ".opendistro-anomaly-detector-jobs";

const FORECASTERS: &str = "opensearch-forecast-config";
const FORECAST_RESULTS: &str = "opensearch-forecast-result";
const FORECAST_STATE: &str = "opensearch-forecast-state";
const FORECAST_CHECKPOINTS: &str = "opensearch-forecast-checkpoint";

/// A configuration that names a category field is a high-cardinality one: it
/// runs a model per category rather than one over the whole stream.
fn split_by_cardinality(configs: &[Value]) -> (usize, usize) {
    let high = configs
        .iter()
        .filter(|c| match c.get("category_field") {
            Some(Value::Array(fields)) => !fields.is_empty(),
            Some(Value::String(f)) => !f.is_empty(),
            _ => false,
        })
        .count();
    (configs.len() - high, high)
}

fn detector_stats(store: &Store) -> Stats {
    let (single, high) = split_by_cardinality(&backing_docs(store, DETECTORS));
    Stats::new(
        vec![
            ("detector_count", json!(single + high)),
            ("single_stream_detector_count", json!(single)),
            ("hc_detector_count", json!(high)),
            ("anomaly_detectors_index_status", backing_index_status(store, DETECTORS)),
            ("anomaly_results_index_status", backing_index_status(store, RESULTS)),
            ("anomaly_detection_state_status", backing_index_status(store, STATE)),
            ("models_checkpoint_index_status", backing_index_status(store, CHECKPOINTS)),
            ("anomaly_detection_job_index_status", backing_index_status(store, JOBS)),
            // the two halves of the plugin report each other's configuration
            // index, as one job index carries both
            ("forecast_config_index_status", backing_index_status(store, FORECASTERS)),
        ],
        vec![
            ("models", json!([])),
            ("model_count", json!(0)),
            ("ad_execute_request_count", json!(0)),
            ("ad_execute_failure_count", json!(0)),
            ("ad_hc_execute_request_count", json!(0)),
            ("ad_hc_execute_failure_count", json!(0)),
            ("ad_executing_batch_task_count", json!(0)),
            ("ad_total_batch_task_execution_count", json!(0)),
            ("ad_batch_task_failure_count", json!(0)),
            ("ad_canceled_batch_task_count", json!(0)),
            ("ad_model_corruption_count", json!(0)),
        ],
    )
}

fn forecast_stats(store: &Store) -> Stats {
    let (single, high) = split_by_cardinality(&backing_docs(store, FORECASTERS));
    Stats::new(
        vec![
            ("forecaster_count", json!(single + high)),
            ("single_stream_forecaster_count", json!(single)),
            ("hc_forecaster_count", json!(high)),
            ("forecast_config_index_status", backing_index_status(store, FORECASTERS)),
            ("forecast_results_index_status", backing_index_status(store, FORECAST_RESULTS)),
            ("forecast_state_index_status", backing_index_status(store, FORECAST_STATE)),
            (
                "forecast_models_checkpoint_index_status",
                backing_index_status(store, FORECAST_CHECKPOINTS),
            ),
            ("anomaly_detection_job_index_status", backing_index_status(store, JOBS)),
        ],
        vec![
            ("models", json!([])),
            ("model_count", json!(0)),
            ("forecast_execute_request_count", json!(0)),
            ("forecast_execute_failure_count", json!(0)),
            ("forecast_hc_execute_request_count", json!(0)),
            ("forecast_hc_execute_failure_count", json!(0)),
            ("forecast_model_corruption_count", json!(0)),
        ],
    )
}

fn answer_stats(stats: Stats, p: &Params, uri: &Uri) -> Response {
    let want = match stats.wanted(uri.path(), stat_in_path(uri)) {
        Ok(want) => want,
        Err(refusal) => return refusal,
    };
    respond(p, stats.answer(crate::cluster::identity().id.as_str(), want.as_deref()))
}

/// `GET _plugins/_anomaly_detection/stats`, with a stat or a node in the path
/// where one was named.
pub async fn detector_stats_api(
    State(store): State<Store>,
    Query(p): Query<Params>,
    uri: Uri,
) -> Response {
    answer_stats(detector_stats(&store), &p, &uri)
}

/// `GET _plugins/_forecast/stats`, the same for the forecasting half.
pub async fn forecast_stats_api(
    State(store): State<Store>,
    Query(p): Query<Params>,
    uri: Uri,
) -> Response {
    answer_stats(forecast_stats(&store), &p, &uri)
}

/// How many configurations there are, and whether one is named `name`.
///
/// `count` answers with how many there are in all; `match` answers whether the
/// name asked for is taken, which is what a console uses before it lets
/// someone create a detector under it.
fn counted(store: &Store, index: &str, p: &Params, by_name: bool) -> Response {
    let configs = backing_docs(store, index);
    if !by_name {
        return respond(p, json!({"count": configs.len(), "match": false}));
    }
    let wanted = p.get("name").map(|s| s.as_str()).unwrap_or_default();
    let taken =
        configs.iter().filter(|c| c.get("name").and_then(|n| n.as_str()) == Some(wanted)).count();
    respond(p, json!({"count": taken, "match": taken > 0}))
}

/// `GET _plugins/_anomaly_detection/detectors/count`
pub async fn detector_count(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    counted(&store, DETECTORS, &p, false)
}

/// `GET _plugins/_anomaly_detection/detectors/match`
pub async fn detector_match(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    counted(&store, DETECTORS, &p, true)
}

/// `GET _plugins/_forecast/forecasters/count`
pub async fn forecaster_count(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    counted(&store, FORECASTERS, &p, false)
}

/// `GET _plugins/_forecast/forecasters/match`
pub async fn forecaster_match(State(store): State<Store>, Query(p): Query<Params>) -> Response {
    counted(&store, FORECASTERS, &p, true)
}
