//! Writes that are acknowledged but not yet committed, and what makes them visible.

use super::*;

impl IdxState {
    /// Open the translog for an index that lives on disk.
    pub(crate) fn open_translog(&mut self) {
        let Some(dir) = self.path.clone() else { return };
        let file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(TRANSLOG));
        self.translog = file.ok().map(std::io::BufWriter::new);
    }

    /// Record a write, so a crash can find it again.
    pub fn log_write(
        &mut self,
        id: &str,
        routing: Option<&str>,
        version: u64,
        seq: u64,
        source: Option<&str>,
    ) {
        use std::io::Write;
        let Some(log) = self.translog.as_mut() else { return };
        let record = serde_json::json!({
            "id": id,
            "routing": routing,
            "version": version,
            "seq": seq,
            "source": source,
        });
        let line = record.to_string();
        let _ = writeln!(log, "{line}");
        self.translog_bytes_since_commit += line.len() as u64 + 1;
        // a record that outgrows the index it stands in for is a recovery that
        // would take longer than the writing did
        if self.translog_bytes_since_commit > TRANSLOG_FLUSH_BYTES {
            let _ = self.apply_ops(None);
            let committed = self.writer.as_mut().map(|w| w.commit().is_ok()).unwrap_or(false);
            if committed {
                let _ = self.realtime.reload();
                self.clear_translog();
            }
        }
    }

    /// Put what has been recorded where a crash cannot lose it.
    ///
    /// Once per request rather than once per document: a bulk of ten thousand
    /// is one write to answer for, the way OpenSearch counts it too.
    pub fn sync_translog(&mut self) {
        use std::io::Write;
        if self.durability_is_async() {
            return;
        }
        if let Some(log) = self.translog.as_mut() {
            let _ = log.flush();
            let _ = log.get_ref().sync_data();
        }
    }

    /// `index.translog.durability: async` asks for speed over the guarantee:
    /// the record is written but not forced, and a crash may lose it.
    fn durability_is_async(&self) -> bool {
        self.setting("translog.durability")
            .map(|v| v.eq_ignore_ascii_case("async"))
            .unwrap_or(false)
    }

    /// Everything written is in the index and on disk: the record is spent.
    pub(crate) fn clear_translog(&mut self) {
        use std::io::Write;
        let Some(dir) = self.path.clone() else { return };
        if let Some(log) = self.translog.as_mut() {
            let _ = log.flush();
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(dir.join(TRANSLOG));
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
        crate::search::routing_shard(routing, self.shard_count().max(1))
    }

    /// Which shard a document lands on: by the routing it was written with if
    /// it was given one, and by its id otherwise.
    pub fn shard_of_doc(&self, id: &str) -> u64 {
        let routing = self.routing.get(id).map(|s| s.as_str()).unwrap_or(id);
        self.shard_for(routing)
    }

    /// Hold a write until the shard it belongs to is refreshed.
    pub fn queue_op(&mut self, shard: u64, op: PendingOp) {
        self.deferred.push((shard, op));
        if self.deferred.len() >= DEFERRED_MAX_OPS {
            let _ = self.apply_ops(None);
        }
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
        let id_field = self.fields.id;
        let w = self.writer()?;
        for (_, op) in go {
            match op {
                PendingOp::Add(doc) => {
                    w.add_document(*doc)?;
                }
                PendingOp::Delete(id) => {
                    w.delete_term(boostcore::Term::from_field_text(id_field, &id));
                }
            }
        }
        Ok(())
    }

    /// Refresh one shard, which is the only thing a write can force.
    ///
    /// What other shards have queued stays queued, and stays invisible.
    pub fn refresh_shard(&mut self, shard: u64) -> Result<()> {
        self.apply_ops(Some(shard))?;
        if let Some(w) = self.writer.as_mut() {
            w.commit()?;
        }
        self.save_meta();
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
        self.apply_ops(None)?;
        // nothing was ever written, so there is nothing to commit
        if let Some(w) = self.writer.as_mut() {
            w.commit()?;
        }
        self.save_meta();
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
            // nothing can be dropped until the writer has it
            let _ = self.apply_ops(None);
            let committed = self.writer.as_mut().map(|w| w.commit().is_ok()).unwrap_or(false);
            if committed {
                let _ = self.realtime.reload();
                self.pending.clear();
                self.pending_bytes = 0;
                self.clear_translog();
            }
        }
    }
}
