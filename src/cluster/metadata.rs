//! What the cluster manager publishes about indices, taken from its own
//! store, and where their shards are placed.
//!
//! The manager's store is the source of truth for index metadata in this
//! step (6.3); it is read as a snapshot, fingerprinted, and republished
//! whenever the fingerprint moves. Placement is the simplest true thing:
//! every primary started on the manager, every replica unassigned until
//! allocation (6.5) learns to put copies on other nodes.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use serde_json::{Value, json};

use super::clock::Millis;
use super::state::{IndexMetadata, RoutingTable, ShardRouting, ShardState, UnassignedInfo};
use super::transport::NodeId;

/// Where index metadata comes from: the store in production, a map in tests.
pub trait MetadataSource: Send + Sync {
    fn snapshot(&self) -> BTreeMap<String, IndexMetadata>;
    /// `_cluster/voting_config_exclusions`, as (node id, node name)
    fn voting_exclusions(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// A fingerprint of a snapshot: the same metadata gives the same number.
pub fn fingerprint(indices: &BTreeMap<String, IndexMetadata>) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (name, m) in indices {
        name.hash(&mut h);
        m.uuid.hash(&mut h);
        m.state.hash(&mut h);
        m.number_of_shards.hash(&mut h);
        m.number_of_replicas.hash(&mut h);
        m.settings.to_string().hash(&mut h);
        m.mappings.to_string().hash(&mut h);
        m.aliases.to_string().hash(&mut h);
    }
    h.finish()
}

/// Allocation ids kept across publications so a shard copy keeps its
/// identity while it stays where it is.
#[derive(Clone, Debug, Default)]
pub struct Allocations {
    ids: BTreeMap<(String, u32, bool), String>,
}

impl Allocations {
    fn id_for(&mut self, index: &str, shard: u32, primary: bool) -> String {
        self.ids
            .entry((index.to_string(), shard, primary))
            .or_insert_with(|| NodeId::random().0)
            .clone()
    }

    pub fn forget_index(&mut self, index: &str) {
        self.ids.retain(|(i, _, _), _| i != index);
    }
}

/// The routing table for this metadata: primaries on the manager, replicas
/// waiting for a node.
pub fn build_routing(
    indices: &BTreeMap<String, IndexMetadata>,
    manager: &NodeId,
    allocations: &mut Allocations,
    now: Millis,
) -> RoutingTable {
    let mut table = RoutingTable::default();
    for (name, m) in indices {
        let mut shards = BTreeMap::new();
        for shard in 0..m.number_of_shards {
            let mut copies = vec![ShardRouting {
                index: name.clone(),
                shard,
                primary: true,
                state: ShardState::Started,
                node: Some(manager.clone()),
                relocating_node: None,
                allocation_id: Some(allocations.id_for(name, shard, true)),
                unassigned: None,
            }];
            for _ in 0..m.number_of_replicas {
                copies.push(ShardRouting {
                    index: name.clone(),
                    shard,
                    primary: false,
                    state: ShardState::Unassigned,
                    node: None,
                    relocating_node: None,
                    allocation_id: None,
                    unassigned: Some(UnassignedInfo {
                        reason: "INDEX_CREATED".into(),
                        at_millis: m.creation_date.max(now),
                        delayed: false,
                        allocation_status: "no_attempt".into(),
                    }),
                });
            }
            shards.insert(shard, copies);
        }
        table.indices.insert(name.clone(), shards);
    }
    table
}

/// Metadata with the primary terms and in-sync allocations the routing gives it.
pub fn with_terms(mut m: IndexMetadata, routing: &RoutingTable) -> IndexMetadata {
    for shard in 0..m.number_of_shards {
        m.primary_terms.entry(shard).or_insert(1);
        let in_sync: Vec<String> = routing
            .indices
            .get(&m.name)
            .and_then(|s| s.get(&shard))
            .map(|copies| copies.iter().filter_map(|c| c.allocation_id.clone()).collect())
            .unwrap_or_default();
        m.in_sync_allocations.insert(shard, in_sync);
    }
    m
}

/// The store as a metadata source.
pub struct StoreSource(pub crate::store::Store);

impl MetadataSource for StoreSource {
    fn voting_exclusions(&self) -> Vec<(String, String)> {
        self.0
            .voting_exclusions()
            .iter()
            .map(|e| {
                let get = |k: &str| e.get(k).and_then(|v| v.as_str()).unwrap_or("_absent_").to_string();
                (get("node_id"), get("node_name"))
            })
            .collect()
    }

    fn snapshot(&self) -> BTreeMap<String, IndexMetadata> {
        let mut out = BTreeMap::new();
        for name in self.0.resolve("*") {
            let Some(st) = self.0.get(&name) else { continue };
            let g = st.read();
            let settings = g.effective_settings();
            let shards = g.shard_count().max(1) as u32;
            let replicas = g.numeric_setting("number_of_replicas").unwrap_or(1) as u32;
            let creation = settings
                .pointer("/index/creation_date")
                .and_then(|v| v.as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| v.as_u64()))
                .unwrap_or(0);
            out.insert(
                name.clone(),
                IndexMetadata {
                    name: name.clone(),
                    uuid: g.uuid.clone(),
                    version: 1,
                    mapping_version: 1,
                    settings_version: 1,
                    aliases_version: 1,
                    state: if g.closed { "close".into() } else { "open".into() },
                    settings: settings.clone(),
                    mappings: g.mapping.raw.clone(),
                    aliases: json!(g.aliases),
                    number_of_shards: shards,
                    number_of_replicas: replicas,
                    primary_terms: BTreeMap::new(),
                    in_sync_allocations: BTreeMap::new(),
                    creation_date: creation,
                },
            );
        }
        out
    }
}

/// A fixed map, for tests and the simulation.
pub struct MapSource(pub parking_lot::Mutex<BTreeMap<String, IndexMetadata>>);

impl MetadataSource for MapSource {
    fn snapshot(&self) -> BTreeMap<String, IndexMetadata> {
        self.0.lock().clone()
    }
}

#[allow(dead_code)]
fn _v(v: Value) -> Value {
    v
}
