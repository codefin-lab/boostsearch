//! The writer: an index's hold on the resources it needs to accept writes.

use super::*;

/// Where the document versions are written down, beside the index.
const VERSIONS: &str = "_versions.bin";
/// The per-document primary terms. A file of its own rather than a field of
/// each version: the versions are postcard, which reads a record by its shape,
/// and a new field would make every versions file written before unreadable.
pub const TERMS: &str = "_terms.bin";
/// What moved in the versions and terms since they were last written in full.
pub const DOC_META_LOG: &str = "_docmeta.log";
/// Past this the record is replaced by a full write of the maps.
const DOC_META_LOG_MAX: u64 = 64 * 1024 * 1024;

impl IdxState {
    /// Why this index takes no changes, if it takes none.
    ///
    /// A write was held to `blocks.write` and nothing else: a delete went
    /// through a blocked index, a read-only index and a closed one alike,
    /// which are the three states an operator puts an index into precisely
    /// so that it stops changing.
    pub fn change_refusal(&self) -> Option<(&'static str, String)> {
        if self.closed {
            return Some((
                "index_closed_exception",
                format!("closed index [{}] cannot be written to", self.name),
            ));
        }
        if self.knobs.blocks_read_only {
            return Some((
                "cluster_block_exception",
                format!("index [{}] blocked by: [FORBIDDEN/5/index read-only (api)];", self.name),
            ));
        }
        // the block a node puts on an index whose disk is past the flood
        // stage: a refusal to be retried once space is freed, which is why
        // it is a 429 and says so, where the read-only block it was reported
        // as is a 403 a client does not retry
        if self.knobs.blocks_read_only_allow_delete {
            return Some((
                "cluster_block_exception",
                format!(
                    "index [{}] blocked by: [TOO_MANY_REQUESTS/12/disk usage exceeded flood-stage \
                     watermark, index has read-only-allow-delete block];",
                    self.name
                ),
            ));
        }
        if self.knobs.blocks_write {
            return Some((
                "cluster_block_exception",
                format!("index [{}] blocked by: [FORBIDDEN/8/index write (api)];", self.name),
            ));
        }
        None
    }

    /// The status a refusal from `change_refusal` is answered with.
    pub fn refusal_status(kind: &str, why: &str) -> axum::http::StatusCode {
        match kind {
            "index_closed_exception" => axum::http::StatusCode::BAD_REQUEST,
            _ if why.contains("TOO_MANY_REQUESTS/") => axum::http::StatusCode::TOO_MANY_REQUESTS,
            _ => axum::http::StatusCode::FORBIDDEN,
        }
    }

    /// The blocks on this index that stop its metadata changing -- its
    /// settings, its mapping -- as the reference writes them, with the status
    /// the refusal carries. `read_only`, `read_only_allow_delete` and
    /// `metadata` are those blocks; `write` stops documents only.
    pub fn metadata_blocks(&self) -> Vec<(&'static str, u16)> {
        let on = |k: &str| self.setting(k).as_deref() == Some("true");
        let mut out = Vec::new();
        if on("blocks.metadata") {
            out.push(("FORBIDDEN/9/index metadata (api)", 403));
        }
        if on("blocks.read_only") {
            out.push(("FORBIDDEN/5/index read-only (api)", 403));
        }
        if on("blocks.read_only_allow_delete") {
            out.push((
                "TOO_MANY_REQUESTS/12/disk usage exceeded flood-stage watermark, index has \
                 read-only-allow-delete block",
                429,
            ));
        }
        out
    }

    /// Persist the learned field information next to the index so a reopen does
    /// not lose dynamic mappings or the range-narrowing kinds.
    /// Where a document's version had got to, written down.
    ///
    /// The map lives in memory and the translog carries the versions of what
    /// is not committed yet -- so what a restart loses is the version of
    /// every document whose record has been spent. `_version` came back as 1
    /// for a document that had been written ten times, and a caller holding
    /// `?version=10` was told the current version is 1.
    ///
    /// It is written when an index goes quiet and when the node stops, and
    /// not on the write path: this is one entry per document the index has
    /// ever been given, and writing it on every commit -- which is where it
    /// was first put -- is the whole map serialised again for each refresh.
    /// A crash between the last quiet moment and now loses the versions of
    /// what was committed since, which is where this started rather than
    /// somewhere worse.
    pub fn save_versions(&self) {
        let Some(path) = &self.path else { return };
        let Ok(bytes) = postcard::to_allocvec(&self.versions) else { return };
        if let Err(e) = write_atomic(&path.join(VERSIONS), &bytes) {
            tracing::error!("index [{}]: could not write the versions: {e}", self.name);
        }
    }

    pub fn save_terms(&self) {
        let Some(path) = &self.path else { return };
        let target = path.join(TERMS);
        // an index that never failed over has no file, and writes none
        if self.terms.is_empty() {
            if target.exists() {
                let _ = std::fs::remove_file(&target);
            }
            return;
        }
        let Ok(bytes) = postcard::to_allocvec(&self.terms) else { return };
        if let Err(e) = write_atomic(&target, &bytes) {
            tracing::error!("index [{}]: could not write the primary terms: {e}", self.name);
        }
    }

    /// Versions and terms written down in full, and the record of what moved
    /// since then thrown away.
    pub fn save_doc_meta(&mut self) {
        self.save_versions();
        self.save_terms();
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path.join(DOC_META_LOG));
        }
        self.meta_dirty.clear();
    }

    /// Append what moved since the last full write of the versions and terms.
    ///
    /// `_version` came back as 1 for a document written three times, after a
    /// `kill -9` that followed a refresh. A refresh commits the documents and
    /// throws the translog away, and the versions were written down only when
    /// an index went quiet or the node shut down cleanly -- rewriting the whole
    /// map at every refresh is what that avoided. So the translog, the one
    /// record of what the versions had become, was spent before anything else
    /// held it. What moved is written here instead, before the translog goes:
    /// a line per document, forced to disk, replayed over the full maps when
    /// the index opens. It stays as small as the writes since the last full
    /// write, and a full write empties it.
    pub fn append_doc_meta_log(&mut self) -> bool {
        if self.meta_dirty.is_empty() {
            return true;
        }
        let Some(path) = self.path.clone() else {
            self.meta_dirty.clear();
            return true;
        };
        let at = path.join(DOC_META_LOG);
        // a record that has grown past the maps it stands in for is replaced
        // by the maps
        if std::fs::metadata(&at).map(|m| m.len()).unwrap_or(0) > DOC_META_LOG_MAX {
            self.save_doc_meta();
            return true;
        }
        let mut out = String::with_capacity(self.meta_dirty.len() * 48);
        for id in &self.meta_dirty {
            let (version, live) = match self.versions.get(id) {
                Some(m) => (m.version, m.live),
                None => (1, true),
            };
            let line =
                serde_json::json!({"id": id, "v": version, "live": live, "t": self.term_of(id)});
            out.push_str(&line.to_string());
            out.push('\n');
        }
        use std::io::Write;
        let written =
            std::fs::OpenOptions::new().create(true).append(true).open(&at).and_then(|mut f| {
                f.write_all(out.as_bytes())?;
                f.sync_all()
            });
        match written {
            Ok(()) => {
                self.meta_dirty.clear();
                true
            }
            Err(e) => {
                tracing::error!(
                    "index [{}]: could not record the versions that moved: {e}",
                    self.name
                );
                false
            }
        }
    }

    /// Replay what moved since the versions and terms were last written down.
    pub fn replay_doc_meta_log(&mut self, path: &std::path::Path) {
        let Ok(text) = std::fs::read_to_string(path.join(DOC_META_LOG)) else { return };
        for line in text.lines() {
            // a line the crash cut short is the last one, and is skipped
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let Some(id) = v.get("id").and_then(|x| x.as_str()) else { continue };
            let version = v.get("v").and_then(|x| x.as_u64()).unwrap_or(1);
            let live = v.get("live").and_then(|x| x.as_bool()).unwrap_or(true);
            if version > 1 || !live {
                self.versions.insert(id.to_string(), DocMeta { version, live });
            } else {
                self.versions.remove(id);
            }
            let term = v.get("t").and_then(|x| x.as_u64()).unwrap_or(1);
            if term > 1 {
                self.terms.insert(id.to_string(), term);
            } else {
                self.terms.remove(id);
            }
        }
    }

    /// The per-document primary terms, or none -- every document in term 1.
    pub fn load_terms(path: &std::path::Path) -> HashMap<String, u64> {
        std::fs::read(path.join(TERMS))
            .ok()
            .and_then(|b| postcard::from_bytes(&b).ok())
            .unwrap_or_default()
    }

    /// The versions as they were written down, if they were.
    pub fn load_versions(path: &std::path::Path) -> HashMap<String, DocMeta> {
        std::fs::read(path.join(VERSIONS))
            .ok()
            .and_then(|b| postcard::from_bytes(&b).ok())
            .unwrap_or_default()
    }

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
            // an index an operator closed stays closed across a restart: it
            // used to come back open and accepting writes, while whatever
            // closed it -- an operator before a snapshot, a policy's `close`
            // action -- believed it was still shut
            "closed": self.closed,
            // where the sequence numbers had got to: a restart that started
            // again from zero would hand new writes numbers old documents
            // already carry, and a recovery pages by sequence number
            "seq_no": self.seq_no,
            // whether a document here stands for several: a rollup index
            // counted its buckets by documents rather than by what they stand
            // for once the node had been restarted
            "has_doc_count": self.has_doc_count,
        });
        let at = path.join("_meta.json");
        if let Err(e) = write_atomic(&at, meta.to_string().as_bytes()) {
            // an index whose state could not be written is an index that
            // comes back changed, so it is said out loud rather than lost
            tracing::error!("index [{}]: could not write {}: {e}", self.name, at.display());
        }
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

    /// What one segment takes: its files on disk, or for an index held in
    /// memory what the segment's structures add up to.
    pub fn segment_bytes(&self, reader: &boostcore::SegmentReader) -> u64 {
        let Some(dir) = &self.path else {
            return reader.space_usage().map(|u| u.total().get_bytes()).unwrap_or(0);
        };
        let id = reader.segment_id();
        self.index
            .searchable_segment_metas()
            .unwrap_or_default()
            .iter()
            .find(|m| m.id() == id)
            .map(|m| {
                m.list_files()
                    .iter()
                    .filter_map(|f| std::fs::metadata(dir.join(f)).ok())
                    .map(|md| md.len())
                    .sum()
            })
            .unwrap_or(0)
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
        // Whatever is queued has to reach the writer first: the copy kept for
        // a realtime read is cleared below, and it is the only other record
        // of it. A queue the writer would not take is a queue that is still
        // owed -- the failure used to be discarded and the record cleared
        // anyway, so an acknowledged write read as missing until the next
        // refresh brought the deferred ops back.
        if self.apply_ops(None).is_err() {
            return false;
        }
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
            // where the sequence numbers had got to lives in the meta file
            // and in the translog, and nowhere else: throwing the translog
            // away without writing the meta had a restart hand new writes
            // numbers old documents already carry
            self.save_meta();
            self.save_doc_meta();
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
            && let Some((held, taken_at)) = crate::knn::Vectors::load(&path)
        {
            let documents = self.realtime.searcher().num_docs() as usize;
            let fields = self.mapping.vector_fields.len();
            // taken at the state the index is in now, and not short of it
            if taken_at == self.seq_no
                && !held.is_empty()
                && held.len() >= documents.min(documents * fields)
            {
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
            held.save(&path, self.seq_no);
        }
    }
}
