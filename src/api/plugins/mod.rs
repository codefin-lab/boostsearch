//! The read surface of the analytics plugins: machine learning, time-series
//! analysis, search relevance and security analytics.
//!
//! None of these has an engine here. What they do have is a documented shape
//! for a cluster where the plugin is installed and nothing has used it, and
//! that shape is what these answer: registries that are empty because they
//! are, counters at zero because nothing has run, and the status of each
//! backing index read from the store rather than asserted. A client that asks
//! one of these whether there is a model, a detector or an experiment gets a
//! true answer instead of a 501 it cannot interpret.

use super::*;
use axum::http::Uri;

pub mod anomaly;
pub mod ml;
pub mod search_relevance;
pub mod security_analytics;

/// The status a plugin reports for one of its own backing indices.
///
/// The plugins create these lazily, on the first write, and report
/// `non-existent` until then -- which is the state of every one of them here.
/// It is read from the store rather than fixed, so an index that arrives under
/// one of these names -- restored from a snapshot, or created by hand -- is
/// reported as what it is.
pub(crate) fn backing_index_status(store: &Store, name: &str) -> Value {
    let Some(st) = store.get(name) else { return json!("non-existent") };
    let names = [name.to_string()];
    let live = crate::cluster::current_state();
    let status = if live.routing.indices.contains_key(name) {
        live.health_status(Some(&names))
    } else if st.read().numeric_setting("number_of_replicas").unwrap_or(1) > 0 {
        // one node holds one copy of a shard, so a replica asked for is a
        // replica nobody has
        "yellow"
    } else {
        "green"
    };
    json!(status)
}

/// Every document one of these backing indices holds.
pub(crate) fn backing_docs(store: &Store, name: &str) -> Vec<Value> {
    let Some(st) = store.get(name) else { return Vec::new() };
    let g = st.read();
    g.all_ids().into_iter().filter_map(|id| crate::api::read_source(&g, &id)).collect()
}

/// The heap figure these plugins report.
///
/// There is no JVM. The nearest thing, and what the node's own
/// `jvm.mem.heap_used_percent` reports, is how much of the machine's memory
/// the allocator holds for the node's data.
pub(crate) fn heap_usage_percent() -> u64 {
    let (resident, _peak) = crate::api::sysinfo::allocator();
    let total = crate::api::sysinfo::memory().total;
    resident.saturating_mul(100).checked_div(total).unwrap_or(0).min(100)
}

/// The stat a stats path named, where it named one.
///
/// These plugins all take the stat as the segment after `stats`, on the
/// cluster's path and on a node's alike, and a path that stops at `stats` --
/// or leaves a trailing slash behind -- asks for all of them. Reading it off
/// the path keeps one handler for the four shapes the plugin answers on.
pub(crate) fn stat_in_path(uri: &Uri) -> Option<String> {
    let segments: Vec<&str> = uri.path().split('/').filter(|s| !s.is_empty()).collect();
    let at = segments.iter().rposition(|s| *s == "stats")?;
    segments.get(at + 1).map(|s| (*s).to_string())
}

/// A plugin's stats, split the way its API reports them: the figures that
/// belong to the cluster, and the ones each node counts for itself.
///
/// `always` names node figures the reference carries whether or not the path
/// asked for them. `empty_node_entry` says what happens when a path asks for
/// a cluster figure alone: the time-series plugins still name the node that
/// answered, with nothing under it, while machine learning leaves `nodes` out.
pub(crate) struct Stats {
    pub cluster: Vec<(&'static str, Value)>,
    pub node: Vec<(&'static str, Value)>,
    pub always: Vec<(&'static str, Value)>,
    pub empty_node_entry: bool,
}

impl Stats {
    pub fn new(cluster: Vec<(&'static str, Value)>, node: Vec<(&'static str, Value)>) -> Self {
        Self { cluster, node, always: Vec::new(), empty_node_entry: true }
    }

    /// Whether a name is one of these stats at all.
    fn known(&self, want: &str) -> bool {
        self.cluster.iter().chain(self.node.iter()).any(|(k, _)| *k == want)
    }

    /// The whole answer, or the one stat the path named.
    ///
    /// A cluster stat asked for on its own still carries this node's entry,
    /// empty: the answer says which node answered it either way.
    pub fn answer(&self, node: &str, only: Option<&str>) -> Value {
        let keep = |k: &&str| only.is_none_or(|want| want == *k);
        let pick = |from: &[(&'static str, Value)]| -> serde_json::Map<String, Value> {
            from.iter()
                .filter(|(k, _)| keep(k))
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect()
        };
        let mut out = pick(&self.cluster);
        let mut mine = pick(&self.node);
        let any_node_stat = !mine.is_empty();
        for (k, v) in &self.always {
            mine.insert((*k).to_string(), v.clone());
        }
        if any_node_stat || self.empty_node_entry {
            out.insert("nodes".into(), json!({node: Value::Object(mine)}));
        }
        Value::Object(out)
    }

    /// The stat the path asked for, refusing a name this plugin does not have
    /// in the words the reference refuses it: a client that mistypes a stat is
    /// told which name it was.
    pub fn wanted(
        &self,
        path: &str,
        stat: Option<String>,
    ) -> std::result::Result<Option<String>, Response> {
        // a request that stops at `stats`, or leaves a trailing slash behind,
        // asks for all of them
        let Some(want) = stat else { return Ok(None) };
        if !self.known(&want) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("request [{path}] contains unrecognized stat: [{want}]"),
            ));
        }
        Ok(Some(want))
    }
}

#[cfg(test)]
mod stats_tests {
    use super::*;

    fn uri(path: &str) -> Uri {
        path.parse().expect("a path is a uri")
    }

    /// Measured against OpenSearch 3.8.0: the stat is the segment after
    /// `stats` whether a node was named before it or not, and a path that
    /// stops there -- or trails a slash -- asks for all of them.
    #[test]
    fn the_stat_is_read_off_the_path() {
        assert_eq!(stat_in_path(&uri("/_plugins/_ml/stats")), None);
        assert_eq!(stat_in_path(&uri("/_plugins/_ml/stats/")), None);
        assert_eq!(
            stat_in_path(&uri("/_plugins/_ml/stats/ml_model_count")).as_deref(),
            Some("ml_model_count")
        );
        assert_eq!(stat_in_path(&uri("/_plugins/_ml/node-1/stats")), None);
        assert_eq!(
            stat_in_path(&uri("/_plugins/_forecast/node-1/stats/model_count")).as_deref(),
            Some("model_count")
        );
    }

    fn sample() -> Stats {
        Stats::new(vec![("detector_count", json!(0))], vec![("model_count", json!(0))])
    }

    #[test]
    fn one_stat_is_answered_where_it_is_counted() {
        let whole = sample().answer("node-1", None);
        assert_eq!(whole["detector_count"], json!(0));
        assert_eq!(whole["nodes"]["node-1"]["model_count"], json!(0));

        // a cluster figure is the cluster's: the node entry stays, empty
        let cluster = sample().answer("node-1", Some("detector_count"));
        assert_eq!(cluster["detector_count"], json!(0));
        assert_eq!(cluster["nodes"]["node-1"], json!({}));

        // a node figure is the node's, and nothing about the cluster is said
        let node = sample().answer("node-1", Some("model_count"));
        assert!(node.get("detector_count").is_none());
        assert_eq!(node["nodes"]["node-1"]["model_count"], json!(0));
    }

    #[test]
    fn machine_learning_leaves_out_the_node_it_was_not_asked_about() {
        let mut stats = sample();
        stats.empty_node_entry = false;
        stats.always = vec![("algorithms", json!({}))];
        let cluster = stats.answer("node-1", Some("detector_count"));
        assert!(cluster.get("nodes").is_none());
        let node = stats.answer("node-1", Some("model_count"));
        assert_eq!(node["nodes"]["node-1"]["algorithms"], json!({}));
    }

    #[test]
    fn a_stat_the_plugin_does_not_have_is_refused() {
        let stats = sample();
        assert!(stats.wanted("/_plugins/_forecast/stats", None).expect("all of them").is_none());
        let refusal = stats
            .wanted("/_plugins/_forecast/stats/bogus", Some("bogus".into()))
            .expect_err("an unknown stat is refused");
        assert_eq!(refusal.status(), StatusCode::BAD_REQUEST);
    }
}
