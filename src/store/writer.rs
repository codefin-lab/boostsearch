//! The writer: an index's hold on the resources it needs to accept writes.

use super::*;

impl IdxState {
    /// Persist the learned field information next to the index so a reopen does
    /// not lose dynamic mappings or the range-narrowing kinds.
    pub fn save_meta(&self) {
        let Some(path) = &self.path else { return };
        let meta = serde_json::json!({
            "name": self.name,
            "body": {"mappings": self.mapping.raw, "settings": self.settings},
            "dynamic_types": self.dynamic_types,
            "observed_kinds": self.observed_kinds,
            "allocation_id": self.allocation_id,
        });
        let _ = std::fs::write(path.join("_meta.json"), meta.to_string());
    }

    /// Bytes each fast-field column occupies. This is the closest honest
    /// analogue of what OpenSearch reports as fielddata.
    pub fn field_column_bytes(&self) -> HashMap<String, u64> {
        let mut out: HashMap<String, u64> = HashMap::new();
        let searcher = self.reader.searcher();
        for seg in searcher.segment_readers() {
            let ff = seg.fast_fields();
            for (path, _) in self.all_field_types() {
                for prefix in [DYN, RAW] {
                    let col = format!("{prefix}.{path}");
                    if let Ok(bytes) = ff.column_num_bytes(&col) {
                        let n = bytes.get_bytes();
                        if n > 0 {
                            *out.entry(path.clone()).or_insert(0) += n;
                        }
                    }
                }
            }
        }
        out
    }

    pub fn has_writer(&self) -> bool {
        self.writer.is_some()
    }

    /// The writer, created on demand.
    pub fn writer(&mut self) -> Result<&mut IndexWriter> {
        self.last_write = std::time::Instant::now();
        // built on first use and kept until the index goes quiet
        if let Some(writer) = self.writer.as_mut() {
            // NLL cannot see that this borrow ends, so the writer is taken
            // again below rather than returned from here
            let _ = writer;
        } else {
            self.writer = Some(
                self.index
                    .writer_with_num_threads(self.writer_threads.max(1), self.writer_budget)?,
            );
        }
        self.writer.as_mut().ok_or_else(|| anyhow!("index [{}] has no writer", self.name))
    }

    /// Give back the indexing threads and arena for an index that has gone
    /// quiet. The writer is only a cache: committing first makes everything it
    /// held durable, so nothing is lost by dropping it.
    ///
    /// Buffered writes are not a reason to refuse. They were, which meant a bulk
    /// load could never release anything -- the buffer is never empty mid-load,
    /// which is exactly when the writers pile up.
    pub fn release_idle_writer(&mut self, idle_for: std::time::Duration) -> bool {
        if self.writer.is_none() || self.last_write.elapsed() < idle_for {
            return false;
        }
        // whatever is queued has to reach the writer first: the copy kept for a
        // realtime read is cleared below, and it is the only other record of it
        let _ = self.apply_ops(None);
        if let Some(mut w) = self.writer.take() {
            if w.commit().is_err() {
                // could not flush cleanly: keep it rather than lose the writes
                self.writer = Some(w);
                return false;
            }
            let _ = w.wait_merging_threads();
        }
        // The realtime reader has to advance so GET still answers from the index
        // now that the buffer is gone. The search reader deliberately does not:
        // a write must stay invisible to search until an explicit refresh.
        let _ = self.realtime.reload();
        self.pending.clear();
        self.pending_seq.clear();
        self.pending_bytes = 0;
        if self.deferred.is_empty() {
            self.clear_translog();
        }
        release_freed_memory();
        true
    }

    /// A stable identifier for the index's current commit point.
    pub fn commit_id(&self) -> String {
        self.index
            .searchable_segment_ids()
            .ok()
            .and_then(|ids| ids.first().map(|i| i.uuid_string()))
            .unwrap_or_else(|| "0".repeat(22))
    }
}
