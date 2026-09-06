//! The cluster state: what every node agrees the cluster is.
//!
//! One value, versioned, published by the cluster manager and applied by
//! every node: the nodes, the manager, the metadata of every index, the
//! routing table saying where each copy of each shard lives, and the
//! blocks. It is written and read in OpenSearch's shapes so that
//! `_cluster/state`, `_cluster/health` and `_cat/shards` answer as they do.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::transport::NodeId;

/// A node as the cluster knows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryNode {
    pub id: NodeId,
    pub name: String,
    pub ephemeral_id: NodeId,
    pub transport_address: String,
    pub roles: Vec<String>,
    pub attributes: BTreeMap<String, String>,
}

impl DiscoveryNode {
    pub fn is_cluster_manager_eligible(&self) -> bool {
        self.roles.iter().any(|r| r == "cluster_manager" || r == "master")
    }

    pub fn is_data(&self) -> bool {
        self.roles.iter().any(|r| r == "data" || r.starts_with("data_"))
    }

    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "ephemeral_id": self.ephemeral_id.as_str(),
            "transport_address": self.transport_address,
            "attributes": self.attributes,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShardState {
    Unassigned,
    Initializing,
    Started,
    Relocating,
}

impl ShardState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ShardState::Unassigned => "UNASSIGNED",
            ShardState::Initializing => "INITIALIZING",
            ShardState::Started => "STARTED",
            ShardState::Relocating => "RELOCATING",
        }
    }
}

/// Why a copy has no node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnassignedInfo {
    pub reason: String,
    pub at_millis: u64,
    pub delayed: bool,
    pub allocation_status: String,
    /// how many times a node failed to start this copy
    #[serde(default)]
    pub failed_allocations: u64,
    /// what the last failure said
    #[serde(default)]
    pub details: Option<String>,
}

/// One copy of one shard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardRouting {
    pub index: String,
    pub shard: u32,
    pub primary: bool,
    pub state: ShardState,
    pub node: Option<NodeId>,
    pub relocating_node: Option<NodeId>,
    pub allocation_id: Option<String>,
    pub unassigned: Option<UnassignedInfo>,
}

impl ShardRouting {
    pub fn to_json(&self) -> Value {
        let mut o = json!({
            "state": self.state.as_str(),
            "primary": self.primary,
            "searchOnly": false,
            "node": self.node.as_ref().map(|n| n.as_str().to_string()),
            "relocating_node": self.relocating_node.as_ref().map(|n| n.as_str().to_string()),
            "shard": self.shard,
            "index": self.index,
        });
        if let Some(a) = &self.allocation_id {
            o["allocation_id"] = json!({"id": a});
        }
        if matches!(self.state, ShardState::Unassigned | ShardState::Initializing) {
            let kind = if self.relocating_node.is_some() || !self.primary {
                "PEER"
            } else if self.unassigned.as_ref().map(|u| u.reason == "INDEX_CREATED").unwrap_or(true)
            {
                "EMPTY_STORE"
            } else {
                "EXISTING_STORE"
            };
            o["recovery_source"] = json!({"type": kind});
            if self.state == ShardState::Initializing {
                o["expected_shard_size_in_bytes"] = json!(0);
            }
            if let Some(u) = &self.unassigned {
                let mut info = json!({
                    "reason": u.reason,
                    "at": iso_millis(u.at_millis),
                    "delayed": u.delayed,
                    "allocation_status": u.allocation_status,
                });
                if u.failed_allocations > 0 {
                    info["failed_attempts"] = json!(u.failed_allocations);
                }
                if let Some(d) = &u.details {
                    info["details"] = json!(d);
                }
                o["unassigned_info"] = info;
            }
        }
        if self.state == ShardState::Relocating {
            o["expected_shard_size_in_bytes"] = json!(0);
        }
        o
    }
}

/// `2026-09-02T20:49:45.917Z`
pub fn iso_millis(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let s = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        s / 3600,
        (s % 3600) / 60,
        s % 60,
        ms % 1000
    )
}

/// What the cluster knows about one index, apart from where its shards are.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexMetadata {
    pub name: String,
    pub uuid: String,
    pub version: u64,
    pub mapping_version: u64,
    pub settings_version: u64,
    pub aliases_version: u64,
    pub state: String,
    pub settings: Value,
    pub mappings: Value,
    pub aliases: Value,
    pub number_of_shards: u32,
    pub number_of_replicas: u32,
    pub primary_terms: BTreeMap<u32, u64>,
    pub in_sync_allocations: BTreeMap<u32, Vec<String>>,
    pub creation_date: u64,
}

impl IndexMetadata {
    pub fn to_json(&self) -> Value {
        json!({
            "version": self.version,
            "mapping_version": self.mapping_version,
            "settings_version": self.settings_version,
            "aliases_version": self.aliases_version,
            "routing_num_shards": self.number_of_shards,
            "state": self.state,
            "settings": self.settings,
            "mappings": self.mappings,
            "aliases": self.aliases.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
            "primary_terms": self.primary_terms.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<Map<_, _>>(),
            "in_sync_allocations": self.in_sync_allocations.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<Map<_, _>>(),
            "rollover_info": {},
            "system": false,
        })
    }
}

/// The routing table: every copy of every shard of every index.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RoutingTable {
    /// index -> shard -> copies (the primary first)
    pub indices: BTreeMap<String, BTreeMap<u32, Vec<ShardRouting>>>,
}

impl RoutingTable {
    /// The copies a node holds, whatever their state.
    pub fn on_node<'a>(&'a self, node: &'a NodeId) -> impl Iterator<Item = &'a ShardRouting> + 'a {
        self.all().filter(move |c| c.node.as_ref() == Some(node))
    }

    pub fn shards_of(&self, index: &str) -> impl Iterator<Item = &ShardRouting> {
        self.indices.get(index).into_iter().flat_map(|s| s.values().flatten())
    }

    pub fn all(&self) -> impl Iterator<Item = &ShardRouting> {
        self.indices.values().flat_map(|s| s.values().flatten())
    }

    pub fn primary(&self, index: &str, shard: u32) -> Option<&ShardRouting> {
        // while a primary is moving there are two copies marked primary: the
        // one being moved away from is the one that answers, until its target
        // says it is ready
        let copies = self.indices.get(index)?.get(&shard)?;
        copies
            .iter()
            .find(|r| r.primary && matches!(r.state, ShardState::Started | ShardState::Relocating))
            .or_else(|| copies.iter().find(|r| r.primary))
    }

    /// Where a shard's copies are, by node: the `routing_nodes` view.
    pub fn by_node(&self) -> (BTreeMap<NodeId, Vec<&ShardRouting>>, Vec<&ShardRouting>) {
        let mut nodes: BTreeMap<NodeId, Vec<&ShardRouting>> = BTreeMap::new();
        let mut unassigned = Vec::new();
        for r in self.all() {
            match &r.node {
                Some(n) => nodes.entry(n.clone()).or_default().push(r),
                None => unassigned.push(r),
            }
        }
        (nodes, unassigned)
    }

    pub fn to_json(&self) -> Value {
        let mut indices = Map::new();
        for (name, shards) in &self.indices {
            let mut s = Map::new();
            for (n, copies) in shards {
                s.insert(n.to_string(), Value::Array(copies.iter().map(|c| c.to_json()).collect()));
            }
            indices.insert(name.clone(), json!({"shards": s}));
        }
        json!({"indices": indices})
    }
}

/// The whole of it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClusterState {
    pub cluster_name: String,
    pub cluster_uuid: String,
    pub cluster_uuid_committed: bool,
    pub state_uuid: String,
    pub version: u64,
    pub term: u64,
    pub cluster_manager: Option<NodeId>,
    pub nodes: BTreeMap<NodeId, DiscoveryNode>,
    pub last_committed_config: Vec<NodeId>,
    pub last_accepted_config: Vec<NodeId>,
    pub voting_config_exclusions: Vec<Value>,
    pub indices: BTreeMap<String, IndexMetadata>,
    pub routing: RoutingTable,
    /// index -> the blocks on it (write, read_only, metadata, read)
    pub index_blocks: BTreeMap<String, Vec<String>>,
    pub cluster_settings: Value,
    /// indices deleted, by name and uuid, so a node that still holds one
    /// knows to let it go (`index-graveyard`)
    #[serde(default)]
    pub graveyard: Vec<Value>,
    /// what the manager's store keeps besides indices: templates,
    /// component templates, pipelines, stored scripts
    #[serde(default)]
    pub customs: Value,
}

impl ClusterState {
    pub fn empty(cluster_name: &str, cluster_uuid: &str) -> ClusterState {
        ClusterState {
            cluster_name: cluster_name.into(),
            cluster_uuid: cluster_uuid.into(),
            cluster_uuid_committed: false,
            state_uuid: NodeId::random().0,
            version: 0,
            term: 0,
            cluster_manager: None,
            nodes: BTreeMap::new(),
            last_committed_config: Vec::new(),
            last_accepted_config: Vec::new(),
            voting_config_exclusions: Vec::new(),
            indices: BTreeMap::new(),
            routing: RoutingTable::default(),
            index_blocks: BTreeMap::new(),
            cluster_settings: json!({"persistent": {}, "transient": {}}),
            graveyard: Vec::new(),
            customs: json!({}),
        }
    }

    /// A state with a new version and uuid, ready to be changed and published.
    pub fn next(&self) -> ClusterState {
        let mut n = self.clone();
        n.version += 1;
        n.state_uuid = NodeId::random().0;
        n
    }

    pub fn data_nodes(&self) -> Vec<&DiscoveryNode> {
        self.nodes.values().filter(|n| n.is_data()).collect()
    }

    pub fn node_json(&self) -> Value {
        Value::Object(
            self.nodes.iter().map(|(id, n)| (id.as_str().to_string(), n.to_json())).collect(),
        )
    }

    /// The counts `_cluster/health` reports.
    pub fn shard_counts(&self, indices: Option<&[String]>) -> ShardCounts {
        let mut c = ShardCounts::default();
        for r in self.routing.all() {
            if let Some(only) = indices
                && !only.iter().any(|i| i == &r.index)
            {
                continue;
            }
            match r.state {
                ShardState::Started => {
                    c.active += 1;
                    if r.primary {
                        c.active_primary += 1;
                    }
                }
                ShardState::Initializing => c.initializing += 1,
                ShardState::Relocating => {
                    c.relocating += 1;
                    c.active += 1;
                    if r.primary {
                        c.active_primary += 1;
                    }
                }
                ShardState::Unassigned => {
                    c.unassigned += 1;
                    if r.unassigned.as_ref().map(|u| u.delayed).unwrap_or(false) {
                        c.delayed += 1;
                    }
                }
            }
        }
        c
    }

    /// green, yellow, red as OpenSearch decides them.
    pub fn health_status(&self, indices: Option<&[String]>) -> &'static str {
        let mut status = "green";
        for r in self.routing.all() {
            if let Some(only) = indices
                && !only.iter().any(|i| i == &r.index)
            {
                continue;
            }
            if r.state != ShardState::Started && r.state != ShardState::Relocating {
                // the target of a move is being filled while the copy it comes
                // from still answers: the shard is not without a primary
                let moving_here = r.relocating_node.is_some()
                    && r.state == ShardState::Initializing
                    && self.routing.shards_of(&r.index).any(|c| {
                        c.shard == r.shard
                            && c.primary == r.primary
                            && c.state == ShardState::Relocating
                    });
                if moving_here {
                    continue;
                }
                if r.primary {
                    return "red";
                }
                status = "yellow";
            }
        }
        status
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShardCounts {
    pub active: usize,
    pub active_primary: usize,
    pub initializing: usize,
    pub relocating: usize,
    pub unassigned: usize,
    /// unassigned replicas waiting out `index.unassigned.node_left.delayed_timeout`
    pub delayed: usize,
}

impl ShardCounts {
    /// `active_shards_percent_as_number`: active copies over all copies.
    pub fn active_percent(&self) -> f64 {
        let total = self.active + self.initializing + self.unassigned;
        if total == 0 { 100.0 } else { self.active as f64 * 100.0 / total as f64 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_millis_reads_like_opensearch() {
        assert_eq!(iso_millis(1_788_382_185_917), "2026-09-02T20:49:45.917Z");
    }

    #[test]
    fn health_follows_the_copies() {
        let mut s = ClusterState::empty("c", "u");
        let mk = |primary, state| ShardRouting {
            index: "i".into(),
            shard: 0,
            primary,
            state,
            node: None,
            relocating_node: None,
            allocation_id: None,
            unassigned: None,
        };
        s.routing.indices.insert(
            "i".into(),
            BTreeMap::from([(
                0,
                vec![mk(true, ShardState::Started), mk(false, ShardState::Unassigned)],
            )]),
        );
        assert_eq!(s.health_status(None), "yellow");
        assert_eq!(
            s.shard_counts(None),
            ShardCounts {
                active: 1,
                active_primary: 1,
                initializing: 0,
                relocating: 0,
                unassigned: 1,
                delayed: 0,
            }
        );
        s.routing.indices.get_mut("i").unwrap().get_mut(&0).unwrap()[0].state =
            ShardState::Unassigned;
        assert_eq!(s.health_status(None), "red");
    }
}
