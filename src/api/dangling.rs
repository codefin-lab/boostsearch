//! `_dangling` -- index data on disk that the cluster's metadata does not
//! know about.
//!
//! A dangling index is what is left when a node is away while an index is
//! deleted, or when a data directory is moved between clusters: the files are
//! there and nothing in the cluster state claims them. This engine never
//! leaves any -- an index this node holds is an index the cluster knows, and
//! one it lets go of is deleted with its files -- so the list is empty and
//! nothing can be imported or deleted by uuid. Which is exactly the state the
//! reference is in when it has none, and these answer as it does.

use super::*;

/// `GET /_dangling` -- every dangling index every node found.
pub async fn list_dangling_indices(Query(p): Query<Params>) -> Response {
    let live = crate::cluster::current_state();
    let nodes = live.nodes.len().max(1);
    respond(
        &p,
        json!({
            "_nodes": {"total": nodes, "successful": nodes, "failed": 0},
            "cluster_name": crate::cluster::identity().cluster_name,
            "dangling_indices": [],
        }),
    )
}

/// The refusal both writes give: there is no such index to act on.
///
/// The reference looks the uuid up across the cluster before it looks at
/// `accept_data_loss`, so a uuid nobody has is refused for not being there
/// rather than for the missing flag -- and it is a 400, not a 404.
fn no_dangling_index(uuid: &str) -> Response {
    err(
        StatusCode::BAD_REQUEST,
        "illegal_argument_exception",
        format!("No dangling index found for UUID [{uuid}]"),
    )
}

/// `POST /_dangling/{index_uuid}` -- bring a dangling index into the cluster.
pub async fn import_dangling_index(Path(uuid): Path<String>) -> Response {
    no_dangling_index(&uuid)
}

/// `DELETE /_dangling/{index_uuid}` -- forget a dangling index for good.
pub async fn delete_dangling_index(Path(uuid): Path<String>) -> Response {
    no_dangling_index(&uuid)
}
