//! Which documents are live, at which version, under which sequence number.

use super::*;

impl IdxState {
    /// Next version for a document id, and the sequence number of the write.
    ///
    /// `existed` must be the answer the caller already got from `is_live`, so a
    /// write cannot decide "updated" and "version 1" from two different sources
    /// while the id table is still loading.
    /// Record a version the caller chose rather than the next one in sequence.
    ///
    /// External versioning hands the index a number kept somewhere else, so
    /// the index follows it rather than counting for itself.
    pub fn bump_to(&mut self, id: &str, live: bool, version: u64) -> (u64, u64) {
        self.moved_on();
        let fp = id_fingerprint(id);
        self.versions.insert(id.to_string(), DocMeta { version, live });
        if live {
            self.live_ids.insert(fp);
        }
        let seq = self.seq_no;
        self.seq_no += 1;
        (version, seq)
    }

    pub fn bump(&mut self, id: &str, live: bool, existed: bool) -> (u64, u64) {
        self.moved_on();
        let fp = id_fingerprint(id);
        let known = existed || self.versions.contains_key(id);
        let version = if known {
            let m =
                self.versions.entry(id.to_string()).or_insert(DocMeta { version: 1, live: true });
            m.version += 1;
            m.live = live;
            m.version
        } else {
            // brand new: version 1 needs no exact entry, only the fingerprint
            1
        };
        if live {
            self.live_ids.insert(fp);
        } else {
            // a tombstone is recorded exactly; removing the fingerprint could
            // take a colliding id's liveness with it
            self.versions.insert(id.to_string(), DocMeta { version, live: false });
        }
        let seq = self.seq_no;
        self.seq_no += 1;
        (version, seq)
    }

    /// Take the version and sequence number the primary gave a write, for a
    /// copy applying it.
    pub fn set_replicated_version(&mut self, id: &str, version: u64, live: bool, seq: u64) {
        let fp = id_fingerprint(id);
        self.versions.insert(id.to_string(), DocMeta { version, live });
        if live {
            self.live_ids.insert(fp);
        }
        self.seq_no = self.seq_no.max(seq + 1);
    }

    /// Which state of this index its answers belong to. Read into the key of
    /// a cached search, so that anything which moves it on leaves what was
    /// cached before unreachable.
    pub fn generation(&self) -> u64 {
        self.search_gen.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Something changed that an answer could depend on.
    ///
    /// The next number comes from the counter every index draws from, not
    /// from this one plus one: an index that is deleted and made again would
    /// otherwise walk back over numbers it has already used, and find answers
    /// filed under them.
    pub fn moved_on(&self) {
        self.search_gen.store(
            crate::store::next_generation(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    pub fn version_of(&self, id: &str) -> u64 {
        self.versions.get(id).map(|m| m.version).unwrap_or(1)
    }

    /// Is there a live document under this id?
    pub fn is_live(&self, id: &str) -> bool {
        match self.pending.get(id) {
            Some(Some(_)) => true,
            Some(None) => false,
            None => match self.versions.get(id) {
                Some(m) => m.live,
                None => {
                    if !self.ids_loaded.load(std::sync::atomic::Ordering::Relaxed) {
                        // table still filling in after a reopen
                        return self.lookup_id(id);
                    }
                    // a fingerprint miss is authoritative; a hit is confirmed
                    // against the index, since fingerprints can collide
                    self.live_ids.contains(&id_fingerprint(id)) && self.lookup_id(id)
                }
            },
        }
    }

    fn lookup_id(&self, id: &str) -> bool {
        alive_address(&self.realtime.searcher(), self.fields.id, id).is_some()
    }

    /// Scan the committed index for live document ids. Runs off the write lock
    /// so a reopen does not stall startup.
    pub fn scan_ids(reader: &IndexReader, id_field: Field) -> Vec<u64> {
        let mut out = Vec::new();
        let searcher = reader.searcher();
        for seg in searcher.segment_readers() {
            let Ok(Some(col)) = seg.fast_fields().str("_id") else { continue };
            let alive = seg.alive_bitset();
            let mut buf = Vec::new();
            for doc in 0..seg.max_doc() {
                if alive.map(|a| !a.is_alive(doc)).unwrap_or(false) {
                    continue;
                }
                let Some(ord) = col.term_ords(doc).next() else { continue };
                buf.clear();
                if col.ord_to_bytes(ord, &mut buf).unwrap_or(false)
                    && let Ok(id) = std::str::from_utf8(&buf)
                {
                    out.push(id_fingerprint(id));
                }
            }
        }
        let _ = id_field;
        out
    }

    /// Merge a scan result in without overwriting anything written since.
    pub fn absorb_ids(&mut self, scanned: Vec<u64>) {
        for fp in scanned {
            self.live_ids.insert(fp);
        }
        self.ids_loaded.store(true, std::sync::atomic::Ordering::Release);
    }

    /// Every live document's id, in the order the segments hold them.
    pub fn all_ids(&self) -> Vec<String> {
        let searcher = self.realtime.searcher();
        let mut out = Vec::new();
        for reader in searcher.segment_readers() {
            let Ok(col) = reader.fast_fields().str("_id") else { continue };
            let Some(col) = col else { continue };
            let alive = reader.alive_bitset();
            for doc in 0..reader.max_doc() {
                if alive.map(|a| a.is_deleted(doc)).unwrap_or(false) {
                    continue;
                }
                // a document has one id, so there is one ordinal to read
                if let Some(ord) = col.term_ords(doc).next() {
                    let mut buf = String::new();
                    if col.ord_to_str(ord, &mut buf).is_ok() {
                        out.push(buf);
                    }
                }
            }
        }
        // a write not yet visible to the reader is still part of the index
        for (id, held) in &self.pending {
            if held.is_some() && !out.contains(id) {
                out.push(id.clone());
            }
        }
        out
    }

    pub fn next_auto_id(&mut self) -> String {
        self.auto_id += 1;
        format!("auto-{:016x}", self.auto_id)
    }
}

/// Where an id's live document sits, or nothing if there is none.
///
/// One id in one term dictionary is not work to spread over a thread pool:
/// handing it to one and waiting on the answer costs more than the answer
/// does, and a write asks this question once per document. The postings are
/// read where they are, and a document that is there but no longer alive does
/// not count as there.
pub(crate) fn alive_address(
    searcher: &boostcore::Searcher,
    id_field: Field,
    id: &str,
) -> Option<boostcore::DocAddress> {
    use boostcore::DocSet;
    let term = Term::from_field_text(id_field, id);
    for (ord, seg) in searcher.segment_readers().iter().enumerate() {
        let Ok(inv) = seg.inverted_index(id_field) else { continue };
        let Ok(Some(mut postings)) =
            inv.read_postings(&term, boostcore::schema::IndexRecordOption::Basic)
        else {
            continue;
        };
        let alive = seg.alive_bitset();
        loop {
            let doc = postings.doc();
            if doc == boostcore::TERMINATED {
                break;
            }
            if alive.map(|a| a.is_alive(doc)).unwrap_or(true) {
                return Some(boostcore::DocAddress::new(ord as u32, doc));
            }
            postings.advance();
        }
    }
    None
}

