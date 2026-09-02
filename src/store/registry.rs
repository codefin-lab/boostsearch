//! The indices this node has, and how one is made, opened and destroyed.

use super::*;

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    /// Periodically hand back indexing resources for indices that have gone
    /// quiet. With one index this is invisible; with hundreds it is the
    /// difference between 13 MB per index and nothing.
    fn start_writer_reaper(&self) {
        let idle_secs: u64 = std::env::var("BOOSTSEARCH_WRITER_IDLE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);
        if idle_secs == 0 {
            return;
        }
        let store = self.clone();
        std::thread::spawn(move || {
            let idle = std::time::Duration::from_secs(idle_secs);
            loop {
                std::thread::sleep(std::time::Duration::from_secs(idle_secs.max(1) / 2 + 1));
                for name in store.names() {
                    if let Some(st) = store.get(&name)
                        && st.write().release_idle_writer(idle)
                    {
                        store.note_writer_closed(&name);
                    }
                }
            }
        });
    }

    pub fn new() -> Store {
        let store = Store {
            inner: Arc::new(RwLock::new(HashMap::new())),
            data_dir: None,
            executor: shared_executor(),
            live_writers: Arc::new(RwLock::new(Vec::new())),
            cluster_settings: Arc::new(RwLock::new(
                serde_json::json!({"persistent": {}, "transient": {}}),
            )),
            voting_exclusions: Arc::new(RwLock::new(Vec::new())),
            components: Arc::new(RwLock::new(HashMap::new())),
            pits: Arc::new(RwLock::new(HashMap::new())),
            data_streams: Arc::new(RwLock::new(HashMap::new())),
            pipelines: Arc::new(RwLock::new(HashMap::new())),
            ingest_stats: Arc::new(RwLock::new(HashMap::new())),
            repositories: Arc::new(RwLock::new(HashMap::new())),
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            pit_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            templates: Arc::new(RwLock::new(HashMap::new())),
            scrolls: Arc::new(RwLock::new(HashMap::new())),
            scroll_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            tasks: Arc::new(RwLock::new(HashMap::new())),
            task_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            scripts: Arc::new(RwLock::new(HashMap::new())),
        };
        store.start_writer_reaper();
        store
    }

    /// Back indices with mmapped files under `dir`, and reopen whatever is
    /// already there. Keeps the index out of process memory: the OS page cache
    /// holds it instead, and it survives a restart.
    pub fn on_disk(dir: impl AsRef<FsPath>) -> Result<Store> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let store = Store {
            inner: Arc::new(RwLock::new(HashMap::new())),
            data_dir: Some(dir.clone()),
            executor: shared_executor(),
            live_writers: Arc::new(RwLock::new(Vec::new())),
            cluster_settings: Arc::new(RwLock::new(
                serde_json::json!({"persistent": {}, "transient": {}}),
            )),
            voting_exclusions: Arc::new(RwLock::new(Vec::new())),
            components: Arc::new(RwLock::new(HashMap::new())),
            pits: Arc::new(RwLock::new(HashMap::new())),
            data_streams: Arc::new(RwLock::new(HashMap::new())),
            pipelines: Arc::new(RwLock::new(HashMap::new())),
            ingest_stats: Arc::new(RwLock::new(HashMap::new())),
            repositories: Arc::new(RwLock::new(HashMap::new())),
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            pit_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            templates: Arc::new(RwLock::new(HashMap::new())),
            scrolls: Arc::new(RwLock::new(HashMap::new())),
            scroll_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            tasks: Arc::new(RwLock::new(HashMap::new())),
            task_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            scripts: Arc::new(RwLock::new(HashMap::new())),
        };
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let meta_path = entry.path().join("_meta.json");
            let Ok(raw) = std::fs::read_to_string(&meta_path) else { continue };
            let Ok(meta): std::result::Result<Value, _> = serde_json::from_str(&raw) else {
                continue;
            };
            let Some(name) = meta.get("name").and_then(|v| v.as_str()) else { continue };
            let body = meta.get("body").cloned().unwrap_or_else(|| serde_json::json!({}));
            let learned = (meta.get("dynamic_types").cloned(), meta.get("observed_kinds").cloned());
            match store.open_index(name, &body, entry.path()) {
                Ok(()) => {
                    // Rebuild the id table in the background: startup no longer
                    // waits on a full scan of every document.
                    if let Some(st) = store.get(name) {
                        {
                            let mut g = st.write();
                            if let Some(v) = learned.0.and_then(|v| serde_json::from_value(v).ok())
                            {
                                g.dynamic_types = v;
                            }
                            match learned.1.and_then(|v| serde_json::from_value(v).ok()) {
                                Some(v) => g.observed_kinds = v,
                                // no kinds recorded: treat what we learn from
                                // here on as partial and never narrow with it
                                None => g.kinds_complete = false,
                            }
                        }
                        let (reader, id_field, flag) = {
                            let g = st.read();
                            (g.realtime.clone(), g.fields.id, g.ids_loaded.clone())
                        };
                        flag.store(false, std::sync::atomic::Ordering::Release);
                        let st2 = st.clone();
                        std::thread::spawn(move || {
                            let scanned = IdxState::scan_ids(&reader, id_field);
                            st2.write().absorb_ids(scanned);
                        });
                    }
                }
                Err(e) => tracing::warn!("could not reopen index {name}: {e}"),
            }
        }
        store.start_writer_reaper();
        Ok(store)
    }

    /// How many indices may hold a writer at once.
    pub fn writer_limit() -> usize {
        std::env::var("BOOSTSEARCH_MAX_LIVE_WRITERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8)
            .max(1)
    }

    /// Record that `name` now holds a writer, releasing the least recently
    /// written index's writer if that puts us over the limit.
    pub fn note_writer_opened(&self, name: &str) {
        let evict = {
            let mut live = self.live_writers.write();
            live.retain(|n| n != name);
            live.push(name.to_string());
            let limit = Self::writer_limit();
            if live.len() > limit { Some(live.remove(0)) } else { None }
        };
        if let Some(victim) = evict
            && let Some(st) = self.get(&victim)
        {
            st.write().release_idle_writer(std::time::Duration::ZERO);
        }
    }

    pub fn note_writer_closed(&self, name: &str) {
        self.live_writers.write().retain(|n| n != name);
    }

    fn index_path(&self, name: &str) -> Option<PathBuf> {
        // an empty name would join to the data directory itself, and deleting
        // an index must never take the whole data directory with it
        let dir = dir_name(name);
        if dir.is_empty() {
            return None;
        }
        self.data_dir.as_ref().map(|d| d.join(dir))
    }

    pub fn exists(&self, name: &str) -> bool {
        self.inner.read().contains_key(name)
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.inner.read().keys().cloned().collect();
        v.sort();
        v
    }

    /// Is this name an alias rather than an index of its own?
    pub fn is_alias(&self, name: &str) -> bool {
        self.inner.read().values().any(|st| st.read().aliases.contains_key(name))
    }

    pub fn get(&self, name: &str) -> Option<Arc<RwLock<IdxState>>> {
        if let Some(s) = self.inner.read().get(name) {
            return Some(s.clone());
        }
        // alias lookup
        let guard = self.inner.read();
        for st in guard.values() {
            if st.read().aliases.contains_key(name) {
                return Some(st.clone());
            }
        }
        None
    }

    /// Resolve an index expression (`test`, `test*`, `_all`, `a,b`) to concrete indices.
    /// Which indices a name or pattern addresses.
    ///
    /// Closed indices are included: most APIs -- delete, stats, the cat
    /// endpoints -- are meant to see them. Searching is the exception, and
    /// asks for `resolve_open` instead.
    pub fn resolve(&self, expr: &str) -> Vec<String> {
        self.resolve_with(expr, true)
    }

    /// As `resolve`, but a pattern passes over the closed indices, which is
    /// what `expand_wildcards` defaults to when reading documents. A name
    /// given outright still resolves, so the caller can say it is closed
    /// rather than that it does not exist.
    pub fn resolve_open(&self, expr: &str) -> Vec<String> {
        self.resolve_with(expr, false)
    }

    pub fn is_closed(&self, name: &str) -> bool {
        self.get(name).map(|st| st.read().closed).unwrap_or(false)
    }

    /// Every index carrying this alias, in name order.
    pub fn indices_for_alias(&self, alias: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .inner
            .read()
            .iter()
            .filter(|(_, st)| st.read().aliases.contains_key(alias))
            .map(|(name, _)| name.clone())
            .collect();
        out.sort();
        out
    }

    fn resolve_with(&self, expr: &str, include_closed: bool) -> Vec<String> {
        let open_only = |names: Vec<String>| -> Vec<String> {
            if include_closed {
                return names;
            }
            names.into_iter().filter(|n| !self.is_closed(n)).collect()
        };
        if expr.is_empty() || expr == "_all" || expr == "*" {
            return open_only(self.names());
        }
        let mut out = Vec::new();
        for part in expr.split(',') {
            let part = part.trim();
            // a name in angle brackets is a date expression standing for the
            // index of some day, month or year
            let resolved;
            let part = if part.starts_with('<') {
                resolved = resolve_date_math_name(part);
                resolved.as_str()
            } else {
                part
            };
            if part.contains('*') {
                let re = wildcard_to_regex(part);
                // a pattern reaches an index by its own name or by any alias
                // standing in front of it
                for n in open_only(self.names()) {
                    let by_alias = self
                        .get(&n)
                        .map(|st| st.read().aliases.keys().any(|a| re.is_match(a)))
                        .unwrap_or(false);
                    if (re.is_match(&n) || by_alias) && !out.contains(&n) {
                        out.push(n);
                    }
                }
            } else if self.exists(part) {
                if !out.contains(&part.to_string()) {
                    out.push(part.to_string());
                }
            } else {
                // an alias may stand in front of several indices, and names
                // all of them; `get` would answer with whichever it found
                // first, which is how a search over an alias came to miss
                // every index but one
                let mut named = self.indices_for_alias(part);
                if named.is_empty()
                    && let Some(st) = self.get(part)
                {
                    named.push(st.read().name.clone());
                }
                for n in open_only(named) {
                    if !out.contains(&n) {
                        out.push(n);
                    }
                }
            }
        }
        out
    }

    pub fn create(&self, name: &str, body: &Value) -> Result<()> {
        let body = &self.apply_templates(name, body);
        if self.exists(name) {
            return Err(anyhow!("resource_already_exists_exception"));
        }
        // an index whose analysis cannot be built is refused now, rather than
        // when the first document is written to it
        if let Some(settings) = body.get("settings")
            && let Some(complaint) = crate::analysis::Registry::complaint(settings)
        {
            return Err(anyhow!("{complaint}"));
        }
        match self.index_path(name) {
            Some(path) => {
                std::fs::create_dir_all(&path)?;
                std::fs::write(
                    path.join("_meta.json"),
                    serde_json::json!({"name": name, "body": body}).to_string(),
                )?;
                self.open_index(name, body, path.clone())?;
                if let Some(st) = self.get(name) {
                    let mut g = st.write();
                    g.path = Some(path);
                    g.open_translog();
                }
                Ok(())
            }
            None => self.open_index_in_ram(name, body),
        }
    }

    fn open_index(&self, name: &str, body: &Value, path: PathBuf) -> Result<()> {
        let (schema, fields) = build_schema();
        let dir = MmapDirectory::open(&path)?;
        let index = Index::open_or_create(dir, schema)?;
        self.finish_open(name, body, index, fields)?;
        if let Some(st) = self.get(name) {
            let mut g = st.write();
            g.path = Some(path);
            g.open_translog();
        }
        Ok(())
    }

    fn open_index_in_ram(&self, name: &str, body: &Value) -> Result<()> {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        self.finish_open(name, body, index, fields)
    }

    fn finish_open(
        &self,
        name: &str,
        body: &Value,
        mut index: Index,
        fields: Fields,
    ) -> Result<()> {
        index.set_executor(self.executor.clone());
        // one arena per indexing thread; a bigger budget means fewer segment
        // flushes and less merging, at the cost of resident memory
        let writer_budget: usize = std::env::var("BOOSTSEARCH_WRITER_BUDGET_MB")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(64)
            * 1024
            * 1024;
        let writer_threads: usize = std::env::var("BOOSTSEARCH_WRITER_THREADS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2);
        let reader =
            index.reader_builder().reload_policy(boostcore::ReloadPolicy::Manual).try_into()?;
        let realtime =
            index.reader_builder().reload_policy(boostcore::ReloadPolicy::Manual).try_into()?;
        let mapping = body
            .get("mappings")
            .map(Mapping::from_body)
            .unwrap_or_else(|| Mapping::from_body(&serde_json::json!({})));
        let settings = body.get("settings").cloned().unwrap_or_else(|| serde_json::json!({}));
        let aliases: HashMap<String, Value> = body
            .get("aliases")
            .and_then(|a| a.as_object())
            .map(|o| o.iter().map(|(k, v)| (k.clone(), normalize_alias(v))).collect())
            .unwrap_or_default();
        let mut st = IdxState {
            name: name.to_string(),
            restored: false,
            index,
            writer: None,
            writer_threads,
            writer_budget,
            last_write: std::time::Instant::now(),
            reader,
            fields,
            mapping,
            settings,
            analysis: Default::default(),
            aliases,
            closed: false,
            versions: HashMap::new(),
            routing: HashMap::new(),
            uuid: index_uuid(name),
            created_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            live_ids: Default::default(),
            pending: HashMap::new(),
            deferred: Vec::new(),
            pending_seq: HashMap::new(),
            pending_bytes: 0,
            realtime,
            seq_no: 0,
            search_count: std::sync::atomic::AtomicU64::new(0),
            request_cache_miss: std::sync::atomic::AtomicU64::new(0),
            search_groups: RwLock::new(HashMap::new()),
            loaded_fielddata: RwLock::new(std::collections::HashSet::new()),
            auto_id: 0,
            dynamic_types: HashMap::new(),
            seen_shapes: std::collections::HashSet::new(),
            observed_kinds: HashMap::new(),
            kinds_complete: true,
            has_doc_count: false,
            noop_updates: std::sync::atomic::AtomicU64::new(0),
            flushes: std::sync::atomic::AtomicU64::new(0),
            gets: std::sync::atomic::AtomicU64::new(0),
            bytes: std::sync::atomic::AtomicU64::new(0),
            kind_path_buf: String::new(),
            path: None,
            translog_bytes_since_commit: 0,
            translog: None,
            stats: Arc::new(crate::blockstats::StatsCache::default()),
            ids_loaded: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        st.apply_analysis();
        self.inner.write().insert(name.to_string(), Arc::new(RwLock::new(st)));
        Ok(())
    }

    /// Auto-create on first write, the way OpenSearch does.
    pub fn ensure(&self, name: &str) -> Result<Arc<RwLock<IdxState>>> {
        if let Some(s) = self.get(name) {
            return Ok(s);
        }
        self.create(name, &serde_json::json!({}))?;
        self.get(name).ok_or_else(|| anyhow!("index vanished"))
    }

    pub fn delete(&self, name: &str) -> bool {
        let targets = self.resolve(name);
        // Dropping an index waits for its writer, and its writer waits for
        // whatever it is merging. Held under the map lock that every other
        // request needs, that wait is the whole server stopping -- long enough
        // for the listen queue to fill and connections to be refused.
        let dropped: Vec<_> = {
            let mut guard = self.inner.write();
            targets.iter().filter_map(|t| guard.remove(t)).collect()
        };
        let any = !dropped.is_empty();
        drop(dropped);
        for t in &targets {
            if let Some(path) = self.index_path(t) {
                let _ = std::fs::remove_dir_all(path);
            }
        }
        any
    }
}
