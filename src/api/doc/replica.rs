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
/// As `apply_replicated`, but the write wins whatever stands here: a page of
/// a recovery is the primary's truth, and a copy being filled may hold a
/// document of the same version from a life the cluster has forgotten.
pub fn apply_recovered(st: &mut IdxState, op: &ReplicaOp) -> bool {
    st.applied_term = st.applied_term.max(op.term);
    apply_replicated_inner(st, op, true)
}

pub fn apply_replicated(st: &mut IdxState, op: &ReplicaOp) -> bool {
    apply_replicated_inner(st, op, false)
}

fn apply_replicated_inner(st: &mut IdxState, op: &ReplicaOp, force: bool) -> bool {
    // a write from a newer primary always wins: the primary that took over
    // counts a document's versions from the copy it held, which may be a
    // version behind what is here, and comparing versions across terms would
    // leave the two copies holding different values for one document for good
    if force {
    } else if op.term > st.applied_term {
        st.applied_term = op.term;
    } else if let Some(have) = held_version(st, &op.id)
        && have >= op.version
    {
        return false;
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
            // the vector goes beside the copy as it goes beside the primary:
            // a k-NN search answered by this copy found a document and not
            // its vector, or the vector it had before
            if !st.mapping.vector_fields.is_empty() {
                st.vectors.write().write(&st.mapping.vector_fields, &op.id, &indexed);
            }
            let doc = crate::store::make_doc(&st.fields, &st.mapping, &op.id, indexed, raw, op.seq);
            if existed {
                st.queue_op_for(&op.id, shard, crate::store::PendingOp::Delete(op.id.clone()));
            }
            st.queue_op_for(&op.id, shard, crate::store::PendingOp::Add(Box::new(doc)));
            st.bytes.fetch_add(raw.len() as u64, std::sync::atomic::Ordering::Relaxed);
            st.log_write(&op.id, op.routing.as_deref(), op.version, op.seq, Some(raw));
            st.note_pending(&op.id, Some(raw.clone()));
            st.note_pending_seq(&op.id, op.seq);
        }
        None => {
            if existed {
                st.queue_op_for(&op.id, shard, crate::store::PendingOp::Delete(op.id.clone()));
                if !st.mapping.vector_fields.is_empty() {
                    st.vectors.write().forget(&op.id);
                }
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
    // a page of nothing would never move on
    let size = size.max(1);
    let term = crate::cluster::primary_term(&st.name, shard);
    // (seq, id) -> op; the pending table wins over the index for the same id.
    // Two documents may carry one sequence number -- a copy filled from a
    // primary that had been restarted, an index from an older version -- and
    // a table keyed by the number alone would hand over one of them and drop
    // the rest, which is a copy quietly missing documents
    let mut found: BTreeMap<(u64, String), ReplicaOp> = BTreeMap::new();
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
            (seq, id.clone()),
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
    // the realtime reader, not the one search reads: a write is committed
    // ahead of a refresh when the memory it holds grows too large, and it is
    // then in neither the pending table nor the reader a search sees. A copy
    // filled from that reader would be quietly missing those documents.
    let searcher = st.realtime.searcher();
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
    // A page is the `size` smallest sequence numbers from both places at
    // once, cut on a number. The pending table was put in whole and the
    // reader was then read only until the page looked full, so a primary
    // holding more pending writes than a page -- one just back from
    // replaying its translog -- filled the page with those, and the next page
    // began past them: every document the reader held below them was never
    // sent. A copy was filled with two thousand of thirty-eight thousand
    // documents that way, and counted in sync.
    let mut numbers: Vec<u64> =
        found.keys().map(|(seq, _)| *seq).chain(picked.iter().map(|(seq, _, _)| *seq)).collect();
    numbers.sort_unstable();
    let cut = (numbers.len() > size).then(|| numbers[size - 1]);
    let more = cut.map(|c| numbers.last().is_some_and(|last| *last > c)).unwrap_or(false);
    if let Some(cut) = cut {
        found.retain(|(seq, _), _| *seq <= cut);
    }
    for (seq, ord, doc_id) in picked {
        if cut.is_some_and(|c| seq > c) {
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
            (seq, id.to_string()),
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
    }
    let ops: Vec<ReplicaOp> = found.into_values().collect();
    let next = if more { cut.map(|c| c + 1) } else { None };
    (ops, next)
}

#[cfg(test)]
mod scan_paging_tests {
    use super::scan_replicated;

    #[test]
    fn pages_reach_every_document_when_more_are_pending_than_a_page_holds() {
        let store = crate::store::Store::scratch();
        let st = store.ensure("paged").expect("an index");
        let mut g = st.write();
        for i in 0..30 {
            assert!(
                crate::api::doc::write_doc(
                    &mut g,
                    &format!("r{i}"),
                    serde_json::json!({"n": i}),
                    "index"
                )
                .is_ok()
            );
        }
        g.refresh().expect("a refresh");
        // more writes waiting for a refresh than a page holds
        for i in 0..25 {
            assert!(
                crate::api::doc::write_doc(
                    &mut g,
                    &format!("p{i}"),
                    serde_json::json!({"n": i}),
                    "index"
                )
                .is_ok()
            );
        }
        let mut from = 0;
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..100 {
            let (ops, next) = scan_replicated(&g, u32::MAX, from, 10);
            seen.extend(ops.into_iter().map(|o| o.id));
            match next {
                Some(n) if n > from => from = n,
                _ => break,
            }
        }
        assert_eq!(seen.len(), 55, "every document is sent, pending or not");
    }
}
