//! The writer: an index's hold on the resources it needs to accept writes.

use super::*;

impl IdxState {
    /// Persist the learned field information next to the index so a reopen does
    /// not lose dynamic mappings or the range-narrowing kinds.
    pub fn save_meta(&self) {
        // mappings, settings and aliases all travel through here, and each of
        // them can change what a search answers
        self.moved_on();
        let Some(path) = &self.path else { return };
        let meta = serde_json::json!({
            "name": self.name,
            "body": {
                "mappings": self.mapping.raw,
                "settings": self.settings,
                // the names this index also answers to: a restart that forgot
                // them would leave every alias pointing at nothing
                "aliases": self.aliases,
            },
            "dynamic_types": self.dynamic_types,
            "observed_kinds": self.observed_kinds,
            "allocation_id": self.allocation_id,
            // where the sequence numbers had got to: a restart that started
            // again from zero would hand new writes numbers old documents
            // already carry, and a recovery pages by sequence number
            "seq_no": self.seq_no,
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
                for prefix in [DYN, RAW, FIELDDATA] {
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

impl IdxState {
    /// Where this index's vectors are written down.
    pub fn vector_path(&self) -> Option<std::path::PathBuf> {
        self.path.as_ref().map(|p| p.join("vectors.bin"))
    }

    /// Read the vectors back, or work them out again from the documents.
    ///
    /// The file is a shortcut, not the truth: the documents are. A file that
    /// holds fewer vectors than there are documents to hold them -- a crash
    /// between a write and a save -- is thrown away and the whole thing is
    /// read again, which is slower and always right.
    pub fn load_vectors(&mut self) {
        if let Some(path) = self.vector_path()
            && let Some(held) = crate::knn::Vectors::load(&path)
        {
            let documents = self.realtime.searcher().num_docs() as usize;
            let fields = self.mapping.vector_fields.len();
            if !held.is_empty() && held.len() >= documents.min(documents * fields) {
                *self.vectors.write() = held;
                // the graph is not written down -- building it costs less
                // than keeping it right on disk would -- so it is built here
                self.vectors.write().maintain(&self.mapping.vector_fields);
                return;
            }
        }
        self.rebuild_vectors();
    }

    /// Read every document and take the vectors out of it.
    pub fn rebuild_vectors(&mut self) {
        use boostcore::schema::document::Value as _;
        let searcher = self.realtime.searcher();
        let mut held = crate::knn::Vectors::default();
        for segment in searcher.segment_readers() {
            let Ok(store) = segment.get_store_reader(1) else { continue };
            for doc_id in segment.doc_ids_alive() {
                let Ok(doc) = store.get::<boostcore::TantivyDocument>(doc_id) else { continue };
                let Some(id) = doc.get_first(self.fields.id).and_then(|v| v.as_str()) else {
                    continue;
                };
                let Some(raw) = doc.get_first(self.fields.source).and_then(|v| v.as_str()) else {
                    continue;
                };
                let Ok(source) = serde_json::from_str::<serde_json::Value>(raw) else { continue };
                held.write(&self.mapping.vector_fields, id, &source);
            }
        }
        *self.vectors.write() = held;
        self.save_vectors();
    }

    /// Write the vectors down, and build the graphs that are wanted over
    /// them.
    ///
    /// Both happen where an index is made durable rather than during a
    /// search: a search that had to build a graph would hold every other one
    /// out while it did.
    pub fn save_vectors(&self) {
        if self.mapping.vector_fields.is_empty() {
            return;
        }
        let mut held = self.vectors.write();
        held.maintain(&self.mapping.vector_fields);
        if let Some(path) = self.vector_path() {
            held.save(&path);
        }
    }
}
