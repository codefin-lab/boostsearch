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

use super::state::{IndexMetadata, RoutingTable, ShardRouting, ShardState};
use super::transport::NodeId;

/// Where index metadata comes from: the store in production, a map in tests.
pub trait MetadataSource: Send + Sync {
    fn snapshot(&self) -> BTreeMap<String, IndexMetadata>;
    /// `_cluster/voting_config_exclusions`, as (node id, node name)
    fn voting_exclusions(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    /// `_cluster/settings` as stored: `{"persistent": {...}, "transient": {...}}`
    fn cluster_settings(&self) -> Value {
        json!({"persistent": {}, "transient": {}})
    }
}

/// What a data node does with the copies the manager puts on it: the store
/// in production, nothing (every copy starts at once) in the simulation.
pub trait ShardHost: Send + Sync {
    /// Make the copy exist here. `Ok(true)` means it is ready now; `Ok(false)`
    /// means the host will say later, through `Input::ShardDone`; an error
    /// fails the allocation.
    fn start_shard(&self, meta: &IndexMetadata, copy: &ShardRouting) -> Result<bool, String>;
    /// The copy is no longer this node's.
    fn remove_shard(&self, index: &str, shard: u32);
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

/// Metadata with the primary terms and in-sync allocations the routing gives
/// it: a copy is in sync once it is active.
pub fn with_terms(
    mut m: IndexMetadata,
    routing: &RoutingTable,
    terms: &BTreeMap<(String, u32), u64>,
) -> IndexMetadata {
    for shard in 0..m.number_of_shards {
        let term = terms.get(&(m.name.clone(), shard)).copied().unwrap_or(1);
        m.primary_terms.insert(shard, term);
        let in_sync: Vec<String> = routing
            .indices
            .get(&m.name)
            .and_then(|s| s.get(&shard))
            .map(|copies| {
                copies
                    .iter()
                    .filter(|c| matches!(c.state, ShardState::Started | ShardState::Relocating))
                    .filter_map(|c| c.allocation_id.clone())
                    .collect()
            })
            .unwrap_or_default();
        m.in_sync_allocations.insert(shard, in_sync);
    }
    m
}

/// The store as a metadata source, and as the host of the copies the
/// manager puts on this node.
pub struct StoreSource {
    pub store: crate::store::Store,
    /// indices this node created for copies placed on it; the ones it may remove
    created: parking_lot::Mutex<std::collections::BTreeSet<String>>,
}

impl StoreSource {
    pub fn new(store: crate::store::Store) -> StoreSource {
        StoreSource { store, created: parking_lot::Mutex::new(std::collections::BTreeSet::new()) }
    }

    /// Fill a copy from the primary's documents, off this thread; the
    /// runtime hears the result as `Input::ShardDone`.
    fn seed(&self, meta: &IndexMetadata, copy: &ShardRouting) -> Result<bool, String> {
        let Some(aid) = copy.allocation_id.clone() else { return Ok(true) };
        let store = self.store.clone();
        let index = meta.name.clone();
        let shard = copy.shard;
        tokio::spawn(async move {
            let result = super::replication::seed_replica(&store, &index, shard).await;
            if let Some(rt) = super::runtime() {
                rt.shard_done(aid, result);
            }
        });
        Ok(false)
    }
}

impl ShardHost for StoreSource {
    fn start_shard(&self, meta: &IndexMetadata, copy: &ShardRouting) -> Result<bool, String> {
        if self.store.get(&meta.name).is_some() && !self.created.lock().contains(&meta.name) {
            // this node's own index: the primary, already here
            return Ok(true);
        }
        if self.store.get(&meta.name).is_some() {
            // a copy already built here (another shard of the same index):
            // caught up from the primary like a new one
            return self.seed(meta, copy);
        }
        // the index as the manager describes it, minus what the store assigns itself
        let mut settings = meta.settings.clone();
        if let Some(idx) = settings.get_mut("index").and_then(|v| v.as_object_mut()) {
            for k in ["uuid", "creation_date", "provided_name", "version"] {
                idx.remove(k);
            }
        }
        let body = json!({"settings": settings, "mappings": meta.mappings});
        self.store.create(&meta.name, &body).map_err(|e| {
            format!("could not create a local copy of [{}][{}]: {e}", meta.name, copy.shard)
        })?;
        self.created.lock().insert(meta.name.clone());
        self.seed(meta, copy)
    }

    fn remove_shard(&self, index: &str, _shard: u32) {
        if self.created.lock().remove(index) {
            self.store.delete(index);
        }
    }
}

impl MetadataSource for StoreSource {
    fn cluster_settings(&self) -> Value {
        self.store.cluster_settings()
    }

    fn voting_exclusions(&self) -> Vec<(String, String)> {
        self.store
            .voting_exclusions()
            .iter()
            .map(|e| {
                let get =
                    |k: &str| e.get(k).and_then(|v| v.as_str()).unwrap_or("_absent_").to_string();
                (get("node_id"), get("node_name"))
            })
            .collect()
    }

    fn snapshot(&self) -> BTreeMap<String, IndexMetadata> {
        let mut out = BTreeMap::new();
        // copies this node holds for the manager are not this node's own indices
        let created = self.created.lock().clone();
        for name in self.store.resolve("*") {
            if created.contains(&name) {
                continue;
            }
            let Some(st) = self.store.get(&name) else { continue };
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
