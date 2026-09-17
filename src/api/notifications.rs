//! `_plugins/_notifications` -- what a notification channel may be.

use super::*;

/// `GET _plugins/_notifications/features` -- the channel types a caller may
/// configure.
///
/// This is the list a dashboard reads to decide which channel forms to draw.
/// No channel can be configured on this node -- there is no notification
/// configuration index and nothing that would send to a channel -- so the
/// list is empty rather than naming types a configuration would be refused
/// for. `plugin_features` carries the interface hints the plugin publishes
/// alongside it.
pub async fn features(Query(p): Query<Params>) -> Response {
    respond(
        &p,
        json!({
            "allowed_config_type_list": [],
            "plugin_features": {"tooltip_support": "true"},
        }),
    )
}
