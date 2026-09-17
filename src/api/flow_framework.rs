//! `_plugins/_flow_framework` -- the workflow steps a template may name.

use super::*;

/// `GET _plugins/_flow_framework/workflow/_steps` -- the catalogue of steps.
///
/// A client reads this to find out which steps it may put in a workflow
/// template and what each one takes and produces. No workflow is provisioned
/// on this node, so there is no step it could run and the catalogue is empty.
/// Naming the reference's steps here would tell a client it may build a
/// template out of work nothing would carry out.
pub async fn steps(Query(p): Query<Params>) -> Response {
    respond(&p, json!({}))
}
