//! A copy takes what the primary wrote, as the primary wrote it: the same
//! version, sequence number and term, applied only if it is newer than what
//! the copy has. And a copy fills itself from the primary by a scan of the
//! primary's documents in sequence order.

use super::*;
use crate::cluster::replication::ReplicaOp;

/// The version a copy holds for an id: from the version table, or 1 for a
/// document that is there without an entry, or nothing.
fn held_version(st: &IdxState, id: &str) -> Option<u64> {
    if let Some(m) = st.versions.get(id) {
        return Some(m.version);
    }
    if exists_doc(st, id) { Some(1) } else { None }
}

/// Apply one of the primary's writes here. `false` when the copy already
/// holds this version or a newer one.
pub fn apply_replicated(st: &mut IdxState, op: &ReplicaOp) -> bool {
    if let Some(have) = held_version(st, &op.id) {
        if have >= op.version {
            return false;
        }
    }
    let existed = exists_doc(st, &op.id);
    if let Some(r) = &op.routing {
        st.routing.insert(op.id.clone(), r.clone());
    }
    st.set_replicated_version(&op.id, op.version, op.source.is_some(), op.seq);
    let shard = st.shard_of_doc(&op.id);
    match &op.source {
        Some(raw) => {
            let source: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
            // the copy's mapping learns what the primary's learned
            let _ = st.mapping.learn_dynamic(&source);
            let indexed = crate::store::expand_for_indexing(source, &st.mapping);
            st.observe(&indexed);
            let doc = crate::store::make_doc(&st.fields, &op.id, indexed, raw, op.seq);
            if existed {
                st.queue_op(shard, crate::store::PendingOp::Delete(op.id.clone()));
            }
            st.queue_op(shard, crate::store::PendingOp::Add(Box::new(doc)));
            st.bytes.fetch_add(raw.len() as u64, std::sync::atomic::Ordering::Relaxed);
            st.log_write(&op.id, op.routing.as_deref(), op.version, op.seq, Some(raw));
            st.note_pending(&op.id, Some(raw.clone()));
            st.note_pending_seq(&op.id, op.seq);
        }
        None => {
            if existed {
                st.queue_op(shard, crate::store::PendingOp::Delete(op.id.clone()));
            }
            st.log_write(&op.id, None, op.version, op.seq, None);
            st.note_pending(&op.id, None);
            st.note_pending_seq(&op.id, op.seq);
        }
    }
    true
}

/// The documents of one shard from a sequence number on, in sequence
/// order, `size` at a time: what a new copy is filled from. Writes still
/// waiting for a refresh are read from the pending table, which is newer
/// than the index.
pub fn scan_replicated(
    st: &IdxState,
    shard: u32,
    from_seq: u64,
    size: usize,
) -> (Vec<ReplicaOp>, Option<u64>) {
    use std::collections::BTreeMap;
    let term = crate::cluster::primary_term(&st.name, shard);
    // seq -> op; the pending table wins over the index for the same id
    let mut found: BTreeMap<u64, ReplicaOp> = BTreeMap::new();
    let mut pending_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let all = shard == u32::MAX;
    for (id, source) in &st.pending {
        if !all && st.shard_of_doc(id) as u32 != shard {
            continue;
        }
        let seq = st.pending_seq.get(id).copied().unwrap_or(u64::MAX - 1);
        pending_ids.insert(id.as_str());
        if seq < from_seq {
            continue;
        }
        found.insert(
            seq,
            ReplicaOp {
                index: st.name.clone(),
                id: id.clone(),
                routing: st.routing.get(id).cloned(),
                version: st.version_of(id),
                seq,
                term,
                shard,
                source: source.clone(),
            },
        );
    }
    let searcher = st.reader.searcher();
    // the `size` smallest sequence numbers at or past `from_seq`, by address
    let mut picked: Vec<(u64, usize, u32)> = Vec::new();
    for (ord, seg) in searcher.segment_readers().iter().enumerate() {
        let Ok(seqs) = seg.fast_fields().u64("_seq") else { continue };
        for doc_id in seg.doc_ids_alive() {
            let Some(seq) = seqs.first(doc_id) else { continue };
            if seq < from_seq {
                continue;
            }
            picked.push((seq, ord, doc_id));
        }
    }
    picked.sort_unstable();
    let mut more = false;
    let mut taken = 0usize;
    for (seq, ord, doc_id) in picked {
        if found.len() >= size {
            more = true;
            break;
        }
        let Ok(store_reader) = searcher.segment_readers()[ord].get_store_reader(1) else {
            continue;
        };
        let Ok(doc) = store_reader.get::<TantivyDocument>(doc_id) else { continue };
        let Some(id) = doc.get_first(st.fields.id).and_then(|v| v.as_str()) else { continue };
        if pending_ids.contains(id) || (!all && st.shard_of_doc(id) as u32 != shard) {
            continue;
        }
        let Some(raw) = doc.get_first(st.fields.source).and_then(|v| v.as_str()) else { continue };
        found.insert(
            seq,
            ReplicaOp {
                index: st.name.clone(),
                id: id.to_string(),
                routing: st.routing.get(id).cloned(),
                version: st.version_of(id),
                seq,
                term,
                shard,
                source: Some(raw.to_string()),
            },
        );
        taken += 1;
    }
    let _ = taken;
    let ops: Vec<ReplicaOp> = found.into_values().take(size).collect();
    let next = if more || ops.len() >= size { ops.last().map(|o| o.seq + 1) } else { None };
    (ops, next)
}
