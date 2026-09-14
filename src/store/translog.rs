//! Writes that are acknowledged but not yet committed, and what makes them visible.

use super::*;

impl IdxState {
    /// Open the translog for an index that lives on disk.
    pub(crate) fn open_translog(&mut self) {
        let Some(dir) = self.path.clone() else { return };
        let file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(TRANSLOG));
        if let Err(e) = &file {
            self.note_translog_error(format!("the translog could not be opened: {e}"));
        }
        self.translog = file.ok().map(std::io::BufWriter::new);
    }

    /// Keep the first failure until a write is answered for.
    fn note_translog_error(&mut self, why: String) {
        if self.translog_error.is_none() {
            tracing::error!("index [{}]: {why}", self.name);
            self.translog_error = Some(why);
        }
    }

    /// Record a write, so a crash can find it again.
    pub fn log_write(
        &mut self,
        id: &str,
        routing: Option<&str>,
        version: u64,
        seq: u64,
        term: u64,
        source: Option<&str>,
    ) {
        use std::io::Write;
        // An index on disk with no record open would answer for writes a crash
        // takes with it; one held in memory has nothing to record to.
        if self.translog.is_none() {
            if self.path.is_some() {
                self.note_translog_error("no translog is open for this index".into());
            }
            return;
        }
        let Some(log) = self.translog.as_mut() else { return };
        // The document already is JSON. Recording it as a JSON *string* would
        // copy it and escape it a second time -- which, for a bulk of large
        // documents, was most of what recording a write cost. It goes in as
        // the value it is.
        let mut line = String::with_capacity(source.map(|s| s.len() + 96).unwrap_or(96));
        line.push_str("{\"id\":");
        push_json_str(&mut line, id);
        if let Some(routing) = routing {
            line.push_str(",\"routing\":");
            push_json_str(&mut line, routing);
        }
        use std::fmt::Write as _;
        let _ = write!(line, ",\"version\":{version},\"seq\":{seq}");
        if term > 1 {
            let _ = write!(line, ",\"term\":{term}");
        }
        line.push_str(",\"source\":");
        match source {
            Some(source) => line.push_str(source),
            None => line.push_str("null"),
        }
        line.push_str("}\n");
        // A record the disk refused was dropped here and the write answered
        // for anyway: the reference fails the write when its translog does.
        if let Err(e) = log.write_all(line.as_bytes()) {
            self.note_translog_error(format!("a write could not be recorded: {e}"));
            return;
        }
        self.translog_bytes_since_commit += line.len() as u64;
        // a record that outgrows the index it stands in for is a recovery that
        // would take longer than the writing did
        if self.translog_bytes_since_commit > TRANSLOG_FLUSH_BYTES {
            // a queue that could not be handed to the writer is a queue the
            // record still stands for: it is not spent until the commit that
            // took every one of them
            let applied = self.apply_ops(None).is_ok();
            let committed =
                applied && self.writer.as_mut().map(|w| w.commit().is_ok()).unwrap_or(false);
            if committed {
                let _ = self.realtime.reload();
                self.save_meta();
                self.clear_translog();
            }
        }
    }

    /// Put what has been recorded where a crash cannot lose it.
    ///
    /// Once per request rather than once per document: a bulk of ten thousand
    /// is one write to answer for, the way OpenSearch counts it too.
    /// It fails if anything recorded since the last write answered for did
    /// not reach the disk; the caller does not acknowledge the write.
    pub fn sync_translog(&mut self) -> Result<(), String> {
        self.flush_translog(false);
        match self.translog_error.take() {
            Some(why) => Err(why),
            None => Ok(()),
        }
    }

    /// The same, with the interval ignored: a shutdown or a flush forces
    /// whatever `async` was still holding back.
    pub fn flush_translog(&mut self, forced: bool) {
        use std::io::Write;
        // `async` risks the disk's cache, not the process's own memory: what
        // a write left in the buffer goes to the file either way, or a clean
        // shutdown would lose writes that were acknowledged.
        let force = forced
            || !self.durability_is_async()
            || self.last_translog_sync.elapsed()
                >= std::time::Duration::from_millis(self.knobs.sync_interval_ms);
        let Some(log) = self.translog.as_mut() else { return };
        let flushed = log.flush();
        let synced = if force && flushed.is_ok() { sync_file(log.get_ref()) } else { Ok(()) };
        if let Err(e) = flushed {
            self.note_translog_error(format!("the translog could not be written: {e}"));
            return;
        }
        if !force {
            return;
        }
        if let Err(e) = synced {
            self.note_translog_error(format!("the translog could not be forced to disk: {e}"));
            return;
        }
        self.last_translog_sync = std::time::Instant::now();
    }

    /// `index.translog.durability: async` asks for speed over the guarantee:
    /// the record is written but not forced, and a crash may lose it.
    fn durability_is_async(&self) -> bool {
        self.knobs.durability.clone().map(|v| v.eq_ignore_ascii_case("async")).unwrap_or(false)
    }

    /// Everything written is in the index and on disk: the record is spent.
    pub(crate) fn clear_translog(&mut self) {
        use std::io::Write;
        let Some(dir) = self.path.clone() else { return };
        // the versions and terms the translog recorded are written down first:
        // a translog thrown away before that took them with it. When they
        // cannot be written the translog is kept, and replays them.
        if !self.append_doc_meta_log() {
            return;
        }
        if let Some(log) = self.translog.as_mut() {
            let _ = log.flush();
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(dir.join(TRANSLOG));
        // The truncation has to reach the disk before anything relies on it.
        // The meta written just before this one goes through `write_atomic`,
        // which forces; this did not, so the two could land out of order and
        // a record already spent could replay over the commit that replaced
        // it -- an acknowledged write reverted by its own record.
        if let Ok(f) = file.as_ref() {
            let _ = f.sync_all();
        }
        if let Ok(d) = std::fs::File::open(&dir) {
            let _ = d.sync_all();
        }
        self.translog = file.ok().map(std::io::BufWriter::new);
        self.translog_bytes_since_commit = 0;
    }

    /// How much is waiting in the translog, which is what a `_stats` call
    /// reports as the uncommitted translog.
    pub fn translog_bytes(&self) -> u64 {
        self.path
            .as_ref()
            .and_then(|d| std::fs::metadata(d.join(TRANSLOG)).ok())
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Which shard a routing value lands on, the way OpenSearch routes it.
    pub fn shard_for(&self, routing: &str) -> u64 {
        crate::search::routing_shard_in(
            routing,
            self.shard_count().max(1),
            self.numeric_setting("boost_routing_shards"),
        )
    }

    /// Which shard a document lands on: by the routing it was written with if
    /// it was given one, and by its id otherwise.
    pub fn shard_of_doc(&self, id: &str) -> u64 {
        self.shard_of(id, self.routing.get(id).map(|s| s.as_str()))
    }

    /// `index.routing_partition_size`: how many shards one routing value may
    /// spread over. One, the default, is no spreading at all.
    pub fn partition_size(&self) -> u64 {
        self.numeric_setting("routing_partition_size").unwrap_or(1).max(1)
    }

    /// Which shard a document with this id and routing lands on, as the
    /// reference's `generateShardId` works it out: the id alone where there is
    /// no routing, and in a partitioned index the routing's hash moved on by
    /// the id's hash folded into the partition size.
    pub fn shard_of(&self, id: &str, routing: Option<&str>) -> u64 {
        let shards = self.shard_count().max(1);
        let rns = self.numeric_setting("boost_routing_shards");
        match routing {
            None => crate::search::routing_shard_in(id, shards, rns),
            Some(r) => {
                let size = self.partition_size();
                let offset = match size {
                    1 => 0,
                    _ => (crate::search::routing_hash(id) as i64).rem_euclid(size as i64) as i32,
                };
                crate::search::routing_shard_offset(r, offset, shards, rns)
            }
        }
    }

    /// Every shard a search with this routing value has to ask: one, or one
    /// per partition in a partitioned index.
    pub fn shards_for_routing(&self, routing: &str) -> std::collections::BTreeSet<u64> {
        let shards = self.shard_count().max(1);
        let rns = self.numeric_setting("boost_routing_shards");
        (0..self.partition_size())
            .map(|offset| crate::search::routing_shard_offset(routing, offset as i32, shards, rns))
            .collect()
    }

    /// The query clause that keeps a search to the documents these shards
    /// hold, carrying the fold it has to redo per document.
    pub fn on_shards_filter(&self, shards: &std::collections::BTreeSet<u64>) -> Value {
        serde_json::json!({"_bs_on_shards": {
            "shards": shards.iter().collect::<Vec<_>>(),
            "of": self.shard_count().max(1),
            "routing_shards": self.numeric_setting("boost_routing_shards"),
            "partition": self.partition_size(),
        }})
    }

    /// Whether the mapping says no document may be written or read by id
    /// without a routing value.
    pub fn routing_required(&self) -> bool {
        self.mapping
            .raw
            .pointer("/_routing/required")
            .map(|v| v == true || v == "true")
            .unwrap_or(false)
    }

    /// How many live documents each shard holds in what a search can see,
    /// worked out from each document's id and routing the way a write places
    /// it.
    pub fn docs_per_shard(&self) -> Vec<u64> {
        let shards = self.shard_count().max(1) as usize;
        let mut out = vec![0u64; shards];
        let searcher = self.reader.searcher();
        for seg in searcher.segment_readers() {
            let Ok(Some(ids)) = seg.fast_fields().str("_id") else { continue };
            let alive = seg.alive_bitset();
            let mut id = String::new();
            for doc in 0..seg.max_doc() {
                if alive.map(|a| a.is_deleted(doc)).unwrap_or(false) {
                    continue;
                }
                let Some(ord) = ids.term_ords(doc).next() else { continue };
                id.clear();
                if ids.ord_to_str(ord, &mut id).is_ok() {
                    let shard = self.shard_of_doc(&id) as usize;
                    out[shard.min(shards - 1)] += 1;
                }
            }
        }
        out
    }

    /// Hold a write until the shard it belongs to is refreshed.
    pub fn queue_op(&mut self, shard: u64, op: PendingOp) {
        self.deferred.push((shard, op));
        if self.deferred.len() >= DEFERRED_MAX_OPS {
            let _ = self.apply_ops(None);
        }
    }

    /// Queue an operation on a document, under the shard that already holds
    /// this document's queued work if there is one.
    ///
    /// A shard-scoped refresh applies only its own shard's queue, and the
    /// shard a write is filed under is read from the routing at the moment
    /// it is queued. Change a document's routing and its `Delete` lands in a
    /// different queue from the `Add` it was meant to retire: the delete
    /// runs first against nothing, the old copy is handed over later, and
    /// one id has two live documents. Everything queued for an id stays in
    /// one queue, in order, until that queue is handed over.
    pub fn queue_op_for(&mut self, id: &str, shard: u64, op: PendingOp) {
        let shard = self.queued_shard.get(id).copied().unwrap_or(shard);
        self.queued_shard.insert(id.to_string(), shard);
        self.queue_op(shard, op);
    }

    /// Hand the writer what is queued -- for one shard, or for all of them.
    pub(crate) fn apply_ops(&mut self, only: Option<u64>) -> Result<()> {
        if self.deferred.is_empty() {
            return Ok(());
        }
        let (go, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.deferred)
            .into_iter()
            .partition(|(shard, _)| only.map(|one| *shard == one).unwrap_or(true));
        self.deferred = keep;
        // What was handed over is no longer waiting anywhere, so the
        // documents in it are not pinned to that queue any more. Clearing
        // only when *everything* had drained pinned a document to a shard
        // for as long as any other shard had work: a later write to it, with
        // `refresh=true`, was queued under the old shard and the refresh of
        // the new one did not make it visible.
        let handed: std::collections::HashSet<u64> = match only {
            Some(one) => std::iter::once(one).collect(),
            None => self.queued_shard.values().copied().collect(),
        };
        self.queued_shard.retain(|_, shard| !handed.contains(shard));
        let id_field = self.fields.id;
        let w = match self.writer() {
            Ok(w) => w,
            Err(e) => {
                // nothing was handed over, so nothing is lost by keeping it
                self.deferred.extend(go);
                return Err(e);
            }
        };
        let mut left: Vec<(u64, PendingOp)> = Vec::new();
        let mut failure = None;
        let mut rest = go.into_iter();
        for (shard, op) in rest.by_ref() {
            let done = match op {
                PendingOp::Add(doc) => w.add_document(*doc).map(|_| ()),
                PendingOp::Delete(id) => {
                    w.delete_term(boostcore::Term::from_field_text(id_field, &id));
                    Ok(())
                }
            };
            if let Err(e) = done {
                // what the writer did not take is still owed: it stays
                // queued, and the record that stands for it stays unspent
                let _ = shard;
                failure = Some(e);
                left.extend(rest);
                break;
            }
        }
        // what the writer refused is queued again, so it is pinned again
        for (shard, op) in &left {
            if let crate::store::PendingOp::Delete(id) = op {
                self.queued_shard.insert(id.clone(), *shard);
            }
        }
        self.deferred.extend(left);
        match failure {
            Some(e) => Err(e.into()),
            None => Ok(()),
        }
    }

    /// Refresh one shard, which is the only thing a write can force.
    ///
    /// What other shards have queued stays queued, and stays invisible.
    pub fn refresh_shard(&mut self, shard: u64) -> Result<()> {
        let started = std::time::Instant::now();
        let done = self.refresh_one_shard(shard);
        self.counters.refresh.add(started.elapsed().as_nanos() as u64);
        done
    }

    fn refresh_one_shard(&mut self, shard: u64) -> Result<()> {
        self.moved_on();
        self.apply_ops(Some(shard))?;
        if let Some(w) = self.writer.as_mut() {
            w.commit()?;
        }
        self.save_meta();
        self.save_vectors();
        self.reader.reload()?;
        self.realtime.reload()?;
        // what this shard held is in the index now, so the copy kept for a
        // realtime read is no longer the only place it lives
        let mine: Vec<String> =
            self.pending.keys().filter(|id| self.shard_of_doc(id) == shard).cloned().collect();
        for id in mine {
            self.pending.remove(&id);
            self.pending_seq.remove(&id);
        }
        if self.deferred.is_empty() {
            self.clear_translog();
        }
        self.pending_bytes = self
            .pending
            .iter()
            .map(|(id, src)| id.len() + src.as_ref().map(|s| s.len()).unwrap_or(0) + 48)
            .sum();
        Ok(())
    }

    /// Make everything written so far visible to search.
    pub fn refresh(&mut self) -> Result<()> {
        let started = std::time::Instant::now();
        let done = self.refresh_everything();
        self.counters.refresh.add(started.elapsed().as_nanos() as u64);
        done
    }

    /// A refresh a caller asked for, which `_stats` counts apart from the
    /// ones the node does on its own.
    pub fn refresh_external(&mut self) -> Result<()> {
        let started = std::time::Instant::now();
        let done = self.refresh();
        self.counters.refresh_external.add(started.elapsed().as_nanos() as u64);
        done
    }

    fn refresh_everything(&mut self) -> Result<()> {
        self.last_refresh = std::time::Instant::now();
        self.moved_on();
        self.apply_ops(None)?;
        // nothing was ever written, so there is nothing to commit
        if let Some(w) = self.writer.as_mut() {
            w.commit()?;
        }
        self.save_meta();
        self.save_vectors();
        self.reader.reload()?;
        self.realtime.reload()?;
        self.pending.clear();
        self.pending_seq.clear();
        self.pending_bytes = 0;
        // everything acknowledged is in the index now, and the index is on
        // disk: what the translog was holding for a crash is spent
        if self.deferred.is_empty() {
            self.clear_translog();
        }
        Ok(())
    }

    /// Bound how much un-refreshed source we hold in memory. Flushing advances
    /// only the realtime reader, so search visibility is unchanged.
    pub fn note_pending_seq(&mut self, id: &str, seq: u64) {
        self.pending_seq.insert(id.to_string(), seq);
    }

    pub fn note_pending(&mut self, id: &str, source: Option<String>) {
        self.pending_bytes += id.len() + source.as_ref().map(|s| s.len()).unwrap_or(0) + 48;
        self.pending.insert(id.to_string(), source);
        if self.pending_bytes > PENDING_BUDGET_BYTES {
            // the copy kept here is the only record of a queued write, so
            // nothing can be dropped until the writer has it -- and the
            // writer has it only when the queue was handed over whole: a
            // write it refused is still owed, and clearing the record would
            // leave it in memory and nowhere else
            let applied = self.apply_ops(None).is_ok();
            let committed =
                applied && self.writer.as_mut().map(|w| w.commit().is_ok()).unwrap_or(false);
            if committed {
                let _ = self.realtime.reload();
                self.pending.clear();
                // it goes with `pending`, which it stands beside: leaving it
                // behind grew it for the length of a bulk and never freed it
                self.pending_seq.clear();
                self.pending_bytes = 0;
                // the sequence numbers go with it, as they do everywhere
                // else the translog is thrown away
                self.save_meta();
                self.clear_translog();
            }
        }
    }
}

/// One string, spelled the way JSON spells it.
fn push_json_str(out: &mut String, text: &str) {
    match serde_json::to_string(text) {
        Ok(quoted) => out.push_str(&quoted),
        Err(_) => out.push_str("\"\""),
    }
}

/// The translog's durability call: a plain `fsync` on macOS, as Java's
/// `FileChannel.force` is there (Rust's `sync_data` would be the far
/// dearer `F_FULLFSYNC`); `sync_data` elsewhere, where they are the same.
fn sync_file(file: &std::fs::File) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        file.sync_data()
    }
}

#[cfg(test)]
mod translog_failure_tests {
    #[test]
    fn a_write_the_disk_refused_is_not_answered_for() {
        let dir = std::env::temp_dir().join(format!("bs-translog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a directory");
        let store = crate::store::Store::on_disk(&dir).expect("a store");
        let st = store.ensure("refused").expect("an index");
        let mut g = st.write();
        // a record that can be opened but not written to, as a disk that has
        // stopped taking writes is
        let path = dir.join("readonly.log");
        std::fs::write(&path, b"").expect("a file");
        let readonly = std::fs::File::open(&path).expect("read only");
        g.translog = Some(std::io::BufWriter::new(readonly));
        let wrote = crate::api::doc::write_doc(&mut g, "a", serde_json::json!({"n": 1}), "index");
        assert!(wrote.is_ok(), "the write itself goes through");
        assert!(g.sync_translog().is_err(), "the record did not reach the disk");
        // and goes on failing while the disk refuses: the record still holds
        // what it could not write, so no later write is answered for either
        assert!(g.sync_translog().is_err(), "a disk still refusing still fails");
        drop(g);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
