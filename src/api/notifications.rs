//! `_plugins/_notifications` -- what a notification channel may be.

use super::*;

/// `GET _plugins/_notifications/features` -- the channel types a caller may
/// configure.
///
/// This is the list a dashboard reads to decide which channel forms to draw,
/// and it is a description of the interface rather than a claim about this
/// node: these nine names are the channel kinds the notifications API knows
/// how to spell, in the order the reference lists them, so a console draws
/// the same forms here as it draws against OpenSearch.
///
/// What the forms lead to is a different matter. Nothing on this node stores
/// a notification configuration or sends to a channel, so a caller who fills
/// one of these in and posts it to `_plugins/_notifications/configs` is
/// refused -- that path is not answered here. The list says what the shape of
/// a configuration may be, not that one would be kept.
///
/// `plugin_features` carries the interface hints the plugin publishes
/// alongside it.
pub async fn features(Query(p): Query<Params>) -> Response {
    respond(
        &p,
        json!({
            "allowed_config_type_list": [
                "slack",
                "chime",
                "microsoft_teams",
                "webhook",
                "email",
                "sns",
                "ses_account",
                "smtp_account",
                "email_group",
            ],
            "plugin_features": {"tooltip_support": "true"},
        }),
    )
}
