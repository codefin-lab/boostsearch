//! `_plugins/_flow_framework` -- the workflow steps a template may name.

use super::*;

/// `GET _plugins/_flow_framework/workflow/_steps` -- the catalogue of steps.
///
/// A client reads this to find out which steps a workflow template may name
/// and what each one takes and produces. The catalogue describes the template
/// language, not this node's ability to run it: these are the step names the
/// reference's validator accepts, with the same inputs, outputs, plugin
/// requirements and timeouts, in the order the reference lists them, so a
/// client that validates a template against the catalogue gets the same
/// verdict here as it gets against OpenSearch.
///
/// Provisioning is another matter. There is no `_plugins/_flow_framework/
/// workflow` path on this node, so a template built out of these steps can be
/// written and checked against the catalogue but never submitted: the POST
/// that would provision it is refused. Nor does anything here carry out the
/// `opensearch-ml` steps -- the model, connector and agent registries are
/// empty and stay empty.
pub async fn steps(Query(p): Query<Params>) -> Response {
    respond(
        &p,
        json!({
            "update_search_pipeline": {"inputs": ["pipeline_id", "configurations"], "outputs": ["pipeline_id"], "required_plugins": []},
            "update_ingest_pipeline": {"inputs": ["pipeline_id", "configurations"], "outputs": ["pipeline_id"], "required_plugins": []},
            "reindex": {"inputs": ["source_index", "destination_index"], "outputs": ["reindex"], "required_plugins": []},
            "register_model_group": {"inputs": ["name"], "outputs": ["model_group_id", "model_group_status"], "required_plugins": ["opensearch-ml"]},
            "undeploy_model": {"inputs": ["model_id"], "outputs": ["success"], "required_plugins": ["opensearch-ml"]},
            "register_remote_model": {"inputs": ["name", "connector_id"], "outputs": ["model_id", "register_model_status"], "required_plugins": ["opensearch-ml"]},
            "register_local_sparse_encoding_model": {"inputs": ["name", "version", "model_format"], "outputs": ["model_id", "register_model_status", "function_name", "model_content_hash_value", "url"], "required_plugins": ["opensearch-ml"], "timeout": "1m"},
            "create_index": {"inputs": ["index_name", "configurations"], "outputs": ["index_name"], "required_plugins": []},
            "delete_agent": {"inputs": ["agent_id"], "outputs": ["agent_id"], "required_plugins": ["opensearch-ml"]},
            "create_ingest_pipeline": {"inputs": ["pipeline_id", "configurations"], "outputs": ["pipeline_id"], "required_plugins": []},
            "register_local_pretrained_model": {"inputs": ["name", "version", "model_format"], "outputs": ["model_id", "register_model_status"], "required_plugins": ["opensearch-ml"], "timeout": "1m"},
            "update_index": {"inputs": ["index_name", "configurations"], "outputs": ["index_name"], "required_plugins": []},
            "create_tool": {"inputs": ["type"], "outputs": ["tools"], "required_plugins": ["opensearch-ml"]},
            "noop": {"inputs": [], "outputs": [], "required_plugins": []},
            "create_connector": {"inputs": ["name", "version", "parameters", "credential", "actions", "protocol", "description"], "outputs": ["connector_id"], "required_plugins": ["opensearch-ml"], "timeout": "1m"},
            "register_agent": {"inputs": ["name", "type"], "outputs": ["agent_id"], "required_plugins": ["opensearch-ml"]},
            "deploy_model": {"inputs": ["model_id"], "outputs": ["model_id"], "required_plugins": ["opensearch-ml"], "timeout": "15s"},
            "create_search_pipeline": {"inputs": ["pipeline_id", "configurations"], "outputs": ["pipeline_id"], "required_plugins": []},
            "register_local_custom_model": {"inputs": ["name", "version", "model_format", "function_name", "model_content_hash_value", "url", "model_type", "embedding_dimension", "framework_type"], "outputs": ["model_id", "register_model_status"], "required_plugins": ["opensearch-ml"], "timeout": "1m"},
            "delete_connector": {"inputs": ["connector_id"], "outputs": ["connector_id"], "required_plugins": ["opensearch-ml"]},
            "delete_model": {"inputs": ["model_id"], "outputs": ["model_id"], "required_plugins": ["opensearch-ml"]},
        }),
    )
}
