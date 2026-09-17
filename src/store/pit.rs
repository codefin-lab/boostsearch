//! Points in time: the readers a search is held to, kept for as long as the
//! point in time lives.
//!
//! A reader is what the index was at one refresh. Holding it keeps the
//! segments it read and the deletions it knew about, so a document updated or
//! deleted since is still found as it was, and one written since is not. A
//! point in time that only remembered how far the index had got could not do
//! that: an update removes the old version, and nothing below a sequence
//! number brings it back.

use super::*;
use base64::Engine;

/// What a point in time's id carries: the token each of its parts is kept
/// under, and which indices each node holds a part of. The id says where to
/// ask, so a search with it can arrive at any node.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PitId {
    #[serde(rename = "t")]
    pub token: String,
    /// indices by the node holding them; the empty name is a node that is not
    /// part of a cluster
    #[serde(rename = "p")]
    pub parts: BTreeMap<String, Vec<String>>,
}

impl PitId {
    pub fn encode(&self) -> String {
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    /// The id read back, or nothing where it is not one this engine handed out.
    pub fn decode(id: &str) -> Option<PitId> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(id.trim()).ok()?;
        let id: PitId = serde_json::from_slice(&bytes).ok()?;
        (!id.token.is_empty()).then_some(id)
    }

    /// Every index the point in time covers, in the order `_shard_doc` numbers
    /// them.
    pub fn indices(&self) -> Vec<String> {
        let mut all: Vec<String> = self.parts.values().flatten().cloned().collect();
        all.sort();
        all.dedup();
        all
    }

    /// Where an index's documents start in `_shard_doc` order. A sequence
    /// number is only unique inside one index, so each index is given a range
    /// of its own and the ranges do not overlap, wherever the index is held.
    pub fn shard_doc_base(&self, index: &str) -> u64 {
        let slot = self.indices().iter().position(|n| n == index).unwrap_or(0) as u64;
        slot << 40
    }
}

/// One index of a point in time, as it was read.
#[derive(Clone)]
pub struct PitPart {
    pub index: String,
    /// the index the reader belongs to: a new index made under the same name
    /// is not the one the point in time was opened over
    pub uuid: String,
    pub searcher: velocore::Searcher,
    /// where this index's documents start in `_shard_doc` order
    pub shard_doc_base: u64,
}

/// This node's part of a point in time.
#[derive(Clone)]
pub struct PitState {
    /// the id the caller holds
    pub id: String,
    /// the caller who opened it, if this node knows callers apart
    pub owner: Option<String>,
    /// when it may be swept away, moved on by every search that asks
    pub expires_at: std::time::Instant,
    pub keep_alive_ms: u64,
    /// when it was opened, in milliseconds since the epoch
    pub created_ms: u64,
    pub parts: Vec<PitPart>,
    /// opened for a scroll to read through, which is not a point in time the
    /// caller opened: it is not listed, and is let go of with the scroll
    pub for_scroll: bool,
}

impl PitState {
    pub fn names(&self) -> Vec<String> {
        self.parts.iter().map(|p| p.index.clone()).collect()
    }
}

/// Whether the caller now may act on every caller's search contexts, which is
/// what an administrator may do and nobody else.
pub(crate) fn caller_administers(store: &Store) -> bool {
    match crate::security::layer::current_caller() {
        None => true,
        Some(c) => c.unrestricted || c.admin_cert || store.security.may_administer(&c),
    }
}

impl Store {
    /// Open this node's part of a point in time: a reader over each index as
    /// it stands now, kept under the id's token.
    pub fn open_pit_part(
        &self,
        id: &PitId,
        indices: &[String],
        keep_alive_ms: u64,
        created_ms: u64,
        for_scroll: bool,
    ) -> std::result::Result<(), String> {
        self.sweep_contexts();
        // what a search would read now is what the point in time holds
        self.refresh_for_search(indices);
        let mut parts = Vec::with_capacity(indices.len());
        for name in indices {
            let Some(st) = self.get(name) else {
                return Err(name.clone());
            };
            let g = st.read();
            parts.push(PitPart {
                index: name.clone(),
                uuid: g.uuid.clone(),
                searcher: g.reader.searcher(),
                shard_doc_base: id.shard_doc_base(name),
            });
        }
        self.pits.write().insert(
            id.token.clone(),
            PitState {
                id: id.encode(),
                owner: current_owner(),
                expires_at: std::time::Instant::now() + keep_for(keep_alive_ms),
                keep_alive_ms: match keep_alive_ms {
                    0 => DEFAULT_KEEP_ALIVE_MS,
                    asked => asked,
                },
                created_ms,
                parts,
                for_scroll,
            },
        );
        Ok(())
    }

    /// This node's part of a point in time, where it is still open and the
    /// caller's to read. A keep-alive given with the search moves its expiry.
    pub fn read_pit(&self, token: &str, keep_alive_ms: Option<u64>) -> Option<PitState> {
        let mut all = self.pits.write();
        let held = all.get_mut(token)?;
        let now = std::time::Instant::now();
        if held.expires_at <= now || !owner_matches(&held.owner) {
            return None;
        }
        // the keep-alive it was opened with is what it is listed with; a
        // search only moves the expiry on
        if let Some(keep) = keep_alive_ms.filter(|k| *k > 0) {
            held.expires_at = now + keep_for(keep);
        }
        Some(held.clone())
    }

    /// The parts of points in time held here that the caller may see: their
    /// own, or every one for an administrator.
    pub fn visible_pits(&self) -> Vec<PitState> {
        self.sweep_contexts();
        let every = caller_administers(self);
        self.pits
            .read()
            .values()
            .filter(|p| !p.for_scroll && (every || owner_matches(&p.owner)))
            .cloned()
            .collect()
    }

    /// Let go of this node's part of a point in time, if it is the caller's
    /// to let go of; whether there was one.
    pub fn close_pit(&self, token: &str) -> bool {
        let every = caller_administers(self);
        let mut all = self.pits.write();
        match all.get(token) {
            Some(p) if every || owner_matches(&p.owner) => {
                all.remove(token);
                true
            }
            _ => false,
        }
    }

    /// Let go of every part held here that the caller may; the ids let go of.
    pub fn close_visible_pits(&self) -> Vec<String> {
        let every = caller_administers(self);
        let mut all = self.pits.write();
        let gone: Vec<String> = all
            .iter()
            .filter(|(_, p)| !p.for_scroll && (every || owner_matches(&p.owner)))
            .map(|(k, _)| k.clone())
            .collect();
        gone.iter().filter_map(|k| all.remove(k)).map(|p| p.id).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_reads_back_as_itself() {
        let mut parts = BTreeMap::new();
        parts.insert("node-b".to_string(), vec!["b".to_string()]);
        parts.insert("node-a".to_string(), vec!["c".to_string(), "a".to_string()]);
        let id = PitId { token: "tok".into(), parts };
        let text = id.encode();
        assert_eq!(PitId::decode(&text), Some(id.clone()));
        assert_eq!(PitId::decode("not-a-pit-id"), None);
        assert_eq!(PitId::decode(""), None);
        // each index has a range of its own, whichever node holds it
        assert_eq!(id.shard_doc_base("a"), 0);
        assert_eq!(id.shard_doc_base("b"), 1 << 40);
        assert_eq!(id.shard_doc_base("c"), 2 << 40);
    }
}
