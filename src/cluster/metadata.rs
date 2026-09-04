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
    /// Indices deleted through this store since last asked, as graveyard
    /// entries; asking takes them.
    fn tombstones(&self) -> Vec<Value> {
        Vec::new()
    }
    /// Whether there are tombstones to take, without taking them.
    fn has_tombstones(&self) -> bool {
        false
    }
    /// Does this node's copy of the index hold any document?
    ///
    /// An index just made holds none, and the cluster is free to place it
    /// wherever there is room; one with documents belongs where they are.
    fn holds_documents(&self, _index: &str) -> bool {
        true
    }
    /// Templates, component templates, pipelines and scripts.
    fn customs(&self) -> Value {
        json!({})
    }
    /// Take the published metadata of an index this node holds a copy of.
    fn apply_index_metadata(&self, _meta: &IndexMetadata) {}
    /// Take the published customs.
    fn apply_customs(&self, _customs: &Value) {}
    /// Let go of a local index the cluster no longer places here.
    fn drop_local(&self, _index: &str) {}
    /// The index copies this node holds on disk -- name, uuid and the
    /// allocation id the copy was given: what the manager places a lost
    /// primary back on, if the copy was in sync.
    fn held(&self) -> Vec<(String, String, String)> {
        self.snapshot().iter().map(|(n, m)| (n.clone(), m.uuid.clone(), String::new())).collect()
    }
    /// The manager gave this node's copy of the index an allocation id.
    fn note_allocation(&self, _index: &str, _allocation_id: &str) {}
}

/// What a data node does with the copies the manager puts on it: the store
/// in production, nothing (every copy starts at once) in the simulation.
pub trait ShardHost: Send + Sync {
    /// Make the copy exist here. `Ok(true)` means it is ready now; `Ok(false)`
    /// means the host will say later, through `Input::ShardDone`; an error
    /// fails the allocation.
    ///
    /// `primary` is the node holding the primary in the state that placed
    /// this copy: the host fills the copy from there rather than from what
    /// its own node believes, which may be older than the manager's word.
    fn start_shard(
        &self,
        meta: &IndexMetadata,
        copy: &ShardRouting,
        primary: Option<&NodeId>,
    ) -> Result<bool, String>;
    /// The copy is no longer this node's.
    fn remove_shard(&self, index: &str, shard: u32);
    /// This node has just become the primary of a shard in a new term: send
    /// what it holds to the other copies, so a copy that followed the old
    /// primary cannot keep a different value for a document nothing writes
    /// again. OpenSearch calls this the primary/replica resync.
    fn resync(&self, _index: &str, _shard: u32, _term: u64, _to: &[NodeId]) {}
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
    previous: &BTreeMap<u32, Vec<String>>,
    retired: &[(u32, String)],
    reset: &[(u32, String)],
    started: &[(u32, String)],
) -> IndexMetadata {
    for shard in 0..m.number_of_shards {
        let term = terms.get(&(m.name.clone(), shard)).copied().unwrap_or(1);
        m.primary_terms.insert(shard, term);
        // In sync: what was in sync before and was not retired, the primary
        // (it is the source every copy is filled from), and the copies whose
        // nodes have just said they finished filling. A copy is not in sync
        // merely for standing active in the routing: one taken out of the set
        // for missing a write would walk straight back into it at the next
        // publication, and could then be handed the primary.
        let mut in_sync: Vec<String> = previous.get(&shard).cloned().unwrap_or_default();
        // a manager that has no set for this shard yet -- a leader whose
        // committed state predates the index -- starts the set from the
        // primary alone: the copies around it may be behind, and only a copy
        // that says it finished filling has caught up
        let from_nothing = in_sync.is_empty();
        if let Some(copies) = routing.indices.get(&m.name).and_then(|s| s.get(&shard)) {
            for c in copies {
                let fresh = started
                    .iter()
                    .any(|(s, a)| *s == shard && Some(a.as_str()) == c.allocation_id.as_deref());
                if matches!(c.state, ShardState::Started | ShardState::Relocating)
                    && (!from_nothing || c.primary || fresh)
                {
                    if let Some(a) = &c.allocation_id {
                        if !in_sync.contains(a) {
                            in_sync.push(a.clone());
                        }
                    }
                }
            }
        }
        for (s, a) in retired {
            if *s == shard {
                in_sync.retain(|x| x != a);
            }
        }
        for (s, a) in reset {
            if *s == shard {
                in_sync = vec![a.clone()];
            }
        }
        // the set never empties while something was in it: it is the cluster's
        // memory of which copy holds the data, and an empty set would let any
        // copy at all be handed the primary
        if in_sync.is_empty() {
            if let Some(before) = previous.get(&shard) {
                if !before.is_empty() {
                    in_sync = before.clone();
                }
            }
        }
        m.in_sync_allocations.insert(shard, in_sync);
    }
    m
}

/// The store as a metadata source, and as the host of the copies the
/// manager puts on this node.
pub struct StoreSource {
    pub store: crate::store::Store,
    /// how many graveyard entries have been handed to the manager
    tombstones_seen: parking_lot::Mutex<usize>,
}

impl StoreSource {
    pub fn new(store: crate::store::Store) -> StoreSource {
        StoreSource { store, tombstones_seen: parking_lot::Mutex::new(0) }
    }

    /// Fill a copy from the primary's documents, off this thread; the
    /// runtime hears the result as `Input::ShardDone`.
    fn seed(
        &self,
        meta: &IndexMetadata,
        copy: &ShardRouting,
        primary: Option<&NodeId>,
    ) -> Result<bool, String> {
        let Some(aid) = copy.allocation_id.clone() else { return Ok(true) };
        let Some(primary) = primary.cloned() else {
            return Err(format!("[{}] has no primary to fill this copy from", meta.name));
        };
        let store = self.store.clone();
        let index = meta.name.clone();
        let shard = copy.shard;
        let id = aid.clone();
        tokio::spawn(async move {
            let result =
                super::replication::seed_replica(&store, &index, shard, &id, &primary).await;
            if let Some(rt) = super::runtime() {
                rt.shard_done(aid, result);
            }
        });
        Ok(false)
    }
}

impl ShardHost for StoreSource {
    fn start_shard(
        &self,
        meta: &IndexMetadata,
        copy: &ShardRouting,
        primary: Option<&NodeId>,
    ) -> Result<bool, String> {
        // the store holds an index, not a shard of one: the first shard's copy
        // is what is made or filled here, and the rest of the shards are that
        // same index under another number
        if copy.shard > 0 && self.store.get(&meta.name).is_some() {
            return Ok(true);
        }
        if copy.primary && copy.relocating_node.is_none() {
            // a primary placed where its data is (a new index, a promoted
            // copy), or an empty one someone asked for after a loss: made
            // empty here when nothing is here
            if self.store.get(&meta.name).is_none() {
                let mut settings = meta.settings.clone();
                if let Some(idx) = settings.get_mut("index").and_then(|v| v.as_object_mut()) {
                    for k in ["creation_date", "provided_name", "version"] {
                        idx.remove(k);
                    }
                    idx.insert("uuid".into(), json!(meta.uuid));
                }
                let body = json!({"settings": settings, "mappings": meta.mappings, "aliases": meta.aliases});
                self.store.create(&meta.name, &body).map_err(|e| {
                    format!("could not make an empty primary of [{}]: {e}", meta.name)
                })?;
            }
            return Ok(true);
        }
        if self.store.get(&meta.name).is_some() {
            // a copy already built here: caught up from the primary like a new one
            return self.seed(meta, copy, primary);
        }
        // the index as the manager describes it, minus what the store assigns itself
        let mut settings = meta.settings.clone();
        // the copy keeps the index's uuid: it is the same index, held here too
        if let Some(idx) = settings.get_mut("index").and_then(|v| v.as_object_mut()) {
            for k in ["creation_date", "provided_name", "version"] {
                idx.remove(k);
            }
            idx.insert("uuid".into(), json!(meta.uuid));
        }
        let body =
            json!({"settings": settings, "mappings": meta.mappings, "aliases": meta.aliases});
        self.store.create(&meta.name, &body).map_err(|e| {
            format!("could not create a local copy of [{}][{}]: {e}", meta.name, copy.shard)
        })?;
        self.seed(meta, copy, primary)
    }

    fn remove_shard(&self, index: &str, _shard: u32) {
        self.store.drop_local(index);
    }

    fn resync(&self, index: &str, shard: u32, term: u64, to: &[NodeId]) {
        let store = self.store.clone();
        let index = index.to_string();
        let to = to.to_vec();
        tokio::spawn(async move {
            if let Err(why) = super::replication::resync(&store, &index, shard, term, &to).await {
                eprintln!("boostsearch: {why}");
            }
        });
    }
}

impl MetadataSource for StoreSource {
    fn holds_documents(&self, index: &str) -> bool {
        self.store
            .get(index)
            .map(|st| {
                let g = st.read();
                g.reader.searcher().num_docs() > 0 || !g.pending.is_empty()
            })
            .unwrap_or(false)
    }

    fn cluster_settings(&self) -> Value {
        self.store.cluster_settings()
    }

    fn tombstones(&self) -> Vec<Value> {
        let all = self.store.tombstones();
        let all = all.as_array().cloned().unwrap_or_default();
        let mut seen = self.tombstones_seen.lock();
        let fresh: Vec<Value> = all.iter().skip(*seen).cloned().collect();
        *seen = all.len();
        fresh
    }

    fn has_tombstones(&self) -> bool {
        let n = self.store.tombstones().as_array().map(|a| a.len()).unwrap_or(0);
        n > *self.tombstones_seen.lock()
    }

    fn customs(&self) -> Value {
        self.store.customs()
    }

    fn apply_index_metadata(&self, meta: &IndexMetadata) {
        let Some(st) = self.store.get(&meta.name) else { return };
        let mut g = st.write();
        if g.settings != meta.settings {
            g.settings = meta.settings.clone();
            g.refresh_knobs();
        }
        if g.mapping.raw != meta.mappings {
            g.mapping.merge(&meta.mappings);
        }
        let aliases: std::collections::HashMap<String, Value> = meta
            .aliases
            .as_object()
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        if g.aliases != aliases {
            g.aliases = aliases;
        }
        let closed = meta.state == "close";
        if g.closed != closed {
            g.closed = closed;
        }
        g.save_meta();
    }

    fn apply_customs(&self, customs: &Value) {
        if customs.is_object() && *customs != self.store.customs() {
            self.store.replace_customs(customs);
        }
    }

    fn drop_local(&self, index: &str) {
        self.store.drop_local(index);
    }

    fn held(&self) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        for name in self.store.resolve("*") {
            if let Some(st) = self.store.get(&name) {
                let g = st.read();
                out.push((
                    name.clone(),
                    g.uuid.clone(),
                    g.allocation_id.clone().unwrap_or_default(),
                ));
            }
        }
        out
    }

    fn note_allocation(&self, index: &str, allocation_id: &str) {
        if let Some(st) = self.store.get(index) {
            let mut g = st.write();
            if g.allocation_id.as_deref() != Some(allocation_id) {
                g.allocation_id = Some(allocation_id.to_string());
                g.save_meta();
            }
        }
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
        for name in self.store.resolve("*") {
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

/// A fixed map, for tests and the simulation; an index taken out of it is
/// reported as deleted, as the store's graveyard would.
pub struct MapSource(
    pub parking_lot::Mutex<BTreeMap<String, IndexMetadata>>,
    pub parking_lot::Mutex<BTreeMap<String, String>>,
);

impl MapSource {
    pub fn new(indices: BTreeMap<String, IndexMetadata>) -> MapSource {
        MapSource(parking_lot::Mutex::new(indices), parking_lot::Mutex::new(BTreeMap::new()))
    }
}

impl MetadataSource for MapSource {
    fn snapshot(&self) -> BTreeMap<String, IndexMetadata> {
        self.0.lock().clone()
    }

    fn has_tombstones(&self) -> bool {
        let now = self.0.lock();
        self.1.lock().keys().any(|n| !now.contains_key(n))
    }

    fn tombstones(&self) -> Vec<Value> {
        let now: BTreeMap<String, String> =
            self.0.lock().iter().map(|(n, m)| (n.clone(), m.uuid.clone())).collect();
        let mut seen = self.1.lock();
        let gone: Vec<Value> = seen
            .iter()
            .filter(|(n, _)| !now.contains_key(*n))
            .map(|(n, u)| json!({"index": {"index_name": n, "index_uuid": u}, "delete_date_in_millis": 0}))
            .collect();
        *seen = now;
        gone
    }
}

#[allow(dead_code)]
fn _v(v: Value) -> Value {
    v
}
