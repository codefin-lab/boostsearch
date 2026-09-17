//! `_plugins/_security_analytics` -- the read surface a console opens with.
//!
//! Nothing here maps a log source, runs a Sigma rule or fetches a threat-intel
//! feed, so there is no finding, no alert and no indicator of compromise to
//! list. Each of those answers the empty shape the plugin gives for that
//! state, so a console opens on an empty board rather than on an error. The
//! rule categories are the exception, and not really one: they name the
//! detector types the API spells, which is a fact about the interface and the
//! same fact here as in the reference.

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
/// a detector may be built for.
///
/// The category is what the rest of the API spells a detector's `detector_type`
/// with, so this is the enumeration of a request field rather than a count of
/// what is installed: these are the categories the reference names, in its
/// order, and `key` and `display_name` are the same string for each, as they
/// are there.
///
/// A caller who picks one of these is going nowhere. No rule set is installed,
/// nothing maps a log source and there is no detector path on this node, so
/// creating a detector of any of these types is refused; `rules/_search` is
/// refused too. The list says which type names the API knows, not that a rule
/// stands behind one.
pub async fn rule_categories(Query(p): Query<Params>) -> Response {
    // key and display name are the same string for every category the
    // reference answers with, so one list gives both
    let categories: Vec<Value> =
        CATEGORIES.iter().map(|name| json!({"key": name, "display_name": name})).collect();
    respond(&p, json!({"rule_categories": categories}))
}

/// The log categories the reference's own rule set is organised by, in the
/// order it lists them.
const CATEGORIES: [&str; 23] = [
    "s3",
    "others_compliance",
    "github",
    "others_application",
    "dns",
    "gworkspace",
    "others_cloud",
    "others_web",
    "windows",
    "cloudtrail",
    "others_macos",
    "ad_ldap",
    "test_windows",
    "network",
    "apache_access",
    "vpcflow",
    "linux",
    "m365",
    "others_apt",
    "okta",
    "waf",
    "others_proxy",
    "azure",
];

/// `GET _plugins/_security_analytics/threat_intel/alerts`
pub async fn threat_intel_alerts(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"alerts": [], "total_alerts": 0}))
}

/// `GET _plugins/_security_analytics/threat_intel/findings/_search`
pub async fn threat_intel_findings(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"total_findings": 0, "ioc_findings": []}))
}

/// `GET _plugins/_security_analytics/threat_intel/iocs` -- the indicators the
/// configured feeds have delivered.
///
/// This is a count of downloaded rows, not a catalogue, so it stays real. The
/// reference containers answer a few hundred here only because the plugin
/// bootstraps one default feed -- Alienvault's IP reputation list -- and
/// fetches it over the network on startup; the total is whatever that download
/// happened to contain that day, and it drops to nothing on a cluster that
/// cannot reach the feed. No feed is configured here and nothing downloads
/// one, so there are no indicators and the total is zero.
pub async fn threat_intel_iocs(Query(p): Query<Params>) -> Response {
    respond(&p, json!({"total": 0, "iocs": []}))
}
