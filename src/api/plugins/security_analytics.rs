//! `_plugins/_security_analytics` -- the read surface a console opens with.
//!
//! Nothing here maps a log source, runs a Sigma rule or fetches a threat-intel
//! feed, so there is no finding, no alert and no indicator of compromise to
//! list, and no rule category either: the categories the reference lists are
//! the ones its own rule set brings, and no rule set is installed. Each of
//! these answers the empty shape the plugin gives for that state, so a console
//! opens on an empty board rather than on an error.

use super::*;

/// `GET _plugins/_security_analytics/correlations` -- the findings correlated
/// with one another. Nothing detects a finding, so nothing correlates.
pub async fn correlations(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"findings": []}))
}

/// `GET _plugins/_security_analytics/correlationAlerts`
pub async fn correlation_alerts(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"correlationAlerts": [], "total_alerts": 0}))
}

/// `GET _plugins/_security_analytics/rules/categories` -- the log categories
/// the installed rules cover. None is installed, so the list is empty; a
/// console reading it offers no category rather than one with no rules behind
/// it.
pub async fn rule_categories(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"rule_categories": []}))
}

/// `GET _plugins/_security_analytics/threat_intel/alerts`
pub async fn threat_intel_alerts(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"alerts": [], "total_alerts": 0}))
}

/// `GET _plugins/_security_analytics/threat_intel/findings/_search`
pub async fn threat_intel_findings(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"total_findings": 0, "ioc_findings": []}))
}

/// `GET _plugins/_security_analytics/threat_intel/iocs` -- the indicators the
/// configured feeds have delivered. No feed is configured and none is
/// downloaded, so there are none.
pub async fn threat_intel_iocs(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"total": 0, "iocs": []}))
}
