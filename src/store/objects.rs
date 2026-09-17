//! What a node holds beside indices: scrolls, templates, repositories,
//! snapshots, pipelines and data streams.

use super::*;

impl Store {
    /// `size` is how many documents each batch returns; the cursor is placed
    /// after the batch the opening search already delivered.
    #[allow(clippy::too_many_arguments)]
    pub fn open_scroll(
        &self,
        expr: &str,
        body: &Value,
        size: usize,
        after: Option<Vec<Value>>,
        implicit_sort: bool,
        keep_alive_ms: u64,
        pit: String,
    ) -> String {
        self.sweep_contexts();
        let keep = keep_alive_ms;
        let token = random_token();
        // the id names the node holding the scroll, so that the next batch
        // can be asked of any node and still find it
        let id = match crate::cluster::runtime() {
            Some(rt) => format!("velosearch-scroll-{token}.{}", hex_of(rt.local().as_str())),
            None => format!("velosearch-scroll-{token}"),
        };
        self.scrolls.write().insert(
            id.clone(),
            ScrollState {
                expr: expr.to_string(),
                owner: current_owner(),
                expires_at: std::time::Instant::now() + keep_for(keep),
                body: body.clone(),
                offset: size,
                size,
                pit,
                after,
                implicit_sort,
            },
        );
        id
    }

    pub fn read_scroll(&self, id: &str) -> Option<ScrollState> {
        let held = self.scrolls.read().get(id).cloned()?;
        // a context that has run out is gone, and one somebody else opened
        // was never this caller's to read
        if held.expires_at <= std::time::Instant::now() || !owner_matches(&held.owner) {
            return None;
        }
        Some(held)
    }

    pub fn advance_scroll(
        &self,
        id: &str,
        by: usize,
        after: Option<Vec<Value>>,
        keep_alive_ms: u64,
    ) {
        if let Some(s) = self.scrolls.write().get_mut(id) {
            s.offset += by;
            if after.is_some() {
                s.after = after;
            }
            // every batch renews the keep-alive, the way asking again does
            s.expires_at = std::time::Instant::now() + keep_for(keep_alive_ms);
        }
    }

    /// Let go of a scroll, if it is the caller's to let go of: a scroll id is
    /// not a capability anyone who has seen it may spend. An administrator may
    /// let go of anyone's.
    pub fn close_scroll(&self, id: &str) -> bool {
        let every = pit::caller_administers(self);
        let mut all = self.scrolls.write();
        match all.get(id) {
            Some(s) if every || owner_matches(&s.owner) => {
                let pit = s.pit.clone();
                all.remove(id);
                drop(all);
                self.close_scroll_pit(&pit);
                true
            }
            _ => false,
        }
    }

    /// Let go of every scroll the caller may: their own, or every one for an
    /// administrator. How many were let go of.
    pub fn close_all_scrolls(&self) -> usize {
        let every = pit::caller_administers(self);
        let mut all = self.scrolls.write();
        let gone: Vec<String> = all
            .iter()
            .filter(|(_, s)| every || owner_matches(&s.owner))
            .map(|(k, _)| k.clone())
            .collect();
        let pits: Vec<String> = gone.iter().filter_map(|k| all.remove(k)).map(|s| s.pit).collect();
        drop(all);
        for pit in &pits {
            self.close_scroll_pit(pit);
        }
        gone.len()
    }

    /// The point in time a scroll read through, let go of with it where it is
    /// held here; a part on another node runs out on its own.
    fn close_scroll_pit(&self, pit: &str) {
        if let Some(id) = PitId::decode(pit) {
            self.pits.write().remove(&id.token);
        }
    }

    /// The node a scroll id says holds the scroll, where it names one.
    pub fn scroll_owner(id: &str) -> Option<String> {
        let rest = id.strip_prefix("velosearch-scroll-")?;
        let (_, node) = rest.rsplit_once('.')?;
        unhex(node)
    }

    /// Index templates, applied to any index created with a matching name.
    pub fn put_template(&self, name: &str, body: Value) {
        self.templates.write().insert(name.to_string(), body);
    }

    /// The snapshot repositories there are.
    pub fn repositories(&self) -> HashMap<String, Value> {
        self.repositories.read().clone()
    }

    pub fn put_repository(&self, name: &str, body: Value) {
        self.repositories.write().insert(name.to_string(), body);
    }

    pub fn remove_repository(&self, pattern: &str) -> usize {
        let mut repos = self.repositories.write();
        let gone: Vec<String> = repos
            .keys()
            .filter(|k| k.as_str() == pattern || wildcard_to_regex(pattern).is_match(k))
            .cloned()
            .collect();
        for g in &gone {
            repos.remove(g);
            self.snapshots.write().remove(g);
        }
        gone.len()
    }

    /// The snapshots held in one repository.
    pub fn snapshots(&self, repo: &str) -> HashMap<String, Value> {
        self.snapshots.read().get(repo).cloned().unwrap_or_default()
    }

    pub fn put_snapshot(&self, repo: &str, name: &str, body: Value) {
        self.snapshots.write().entry(repo.to_string()).or_default().insert(name.to_string(), body);
    }

    pub fn remove_snapshots(&self, repo: &str, pattern: &str) -> usize {
        let mut all = self.snapshots.write();
        let Some(map) = all.get_mut(repo) else { return 0 };
        let gone: Vec<String> = map
            .keys()
            .filter(|k| k.as_str() == pattern || wildcard_to_regex(pattern).is_match(k))
            .cloned()
            .collect();
        for g in &gone {
            map.remove(g);
        }
        gone.len()
    }

    /// The pipelines of one kind, by name.
    pub fn pipelines(&self, kind: &str) -> HashMap<String, Value> {
        self.pipelines.read().get(kind).cloned().unwrap_or_default()
    }

    pub fn put_pipeline(&self, kind: &str, name: &str, body: Value) {
        self.pipelines.write().entry(kind.to_string()).or_default().insert(name.to_string(), body);
        if kind == "ingest" {
            self.any_ingest_pipeline.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Remove the pipelines of one kind whose names a pattern reaches.
    pub fn remove_pipelines(&self, kind: &str, pattern: &str) -> usize {
        let mut all = self.pipelines.write();
        let Some(map) = all.get_mut(kind) else { return 0 };
        let gone: Vec<String> = map
            .keys()
            .filter(|k| k.as_str() == pattern || wildcard_to_regex(pattern).is_match(k))
            .cloned()
            .collect();
        for g in &gone {
            map.remove(g);
        }
        gone.len()
    }

    /// The data streams there are, each with the template it was made from.
    pub fn data_streams(&self) -> HashMap<String, String> {
        self.data_streams.read().clone()
    }

    /// The backing indices of a data stream, oldest first.
    ///
    /// A stream is a name in front of `.ds-<name>-NNNNNN` indices, and the
    /// newest of them is the one writes go to. Nothing outside the
    /// `_data_stream` endpoints knew that, so a write to the stream made an
    /// ordinary index of the stream's own name and put the documents there
    /// while the backing index stayed empty -- `GET _data_stream` named an
    /// index that never received anything.
    pub fn backing_indices(&self, stream: &str) -> Vec<String> {
        if !self.data_streams.read().contains_key(stream) {
            return Vec::new();
        }
        let prefix = format!(".ds-{stream}-");
        let mut out: Vec<String> =
            self.inner.read().keys().filter(|n| n.starts_with(&prefix)).cloned().collect();
        out.sort();
        out
    }

    /// The stream a backing index belongs to, if it belongs to one.
    pub fn stream_behind(&self, index: &str) -> Option<String> {
        let rest = index.strip_prefix(".ds-")?;
        let (name, _) = rest.rsplit_once('-')?;
        self.data_streams.read().contains_key(name).then(|| name.to_string())
    }

    pub fn add_data_stream(&self, name: &str, template: &str) {
        self.data_streams.write().insert(name.to_string(), template.to_string());
    }

    pub fn remove_data_stream(&self, name: &str) -> Vec<String> {
        let mut streams = self.data_streams.write();
        let gone: Vec<String> = streams
            .keys()
            .filter(|k| k.as_str() == name || wildcard_to_regex(name).is_match(k))
            .cloned()
            .collect();
        for g in &gone {
            streams.remove(g);
        }
        gone
    }

    pub fn get_templates(&self) -> HashMap<String, Value> {
        self.templates.read().clone()
    }

    pub fn delete_template(&self, name: &str) -> bool {
        let mut t = self.templates.write();
        let pats: Vec<String> = t
            .keys()
            .filter(|k| k.as_str() == name || wildcard_to_regex(name).is_match(k))
            .cloned()
            .collect();
        let hit = !pats.is_empty();
        for p in pats {
            t.remove(&p);
        }
        hit
    }

    /// Merge every template whose pattern matches, lowest order first, so an
    /// index picks up the mappings and settings it was meant to be born with.
    pub(crate) fn apply_templates(&self, index: &str, body: &Value) -> Value {
        // A backing index is made from its data stream's template, which
        // names the stream's patterns and not `.ds-<stream>-NNNNNN`: matched
        // by its own name it matched nothing, and every backing index came
        // out with no mappings and the default settings while
        // `_simulate_index` promised the template's. The stream's name is
        // what the template is matched against.
        if let Some(stream) = stream_of_backing_name(index)
            && let Some(made) = self.apply_stream_template(stream, index, body)
        {
            return made;
        }
        let templates = self.templates.read();
        let mut matched: Vec<(i64, &String, &Value)> = templates
            .iter()
            .filter(|(_, t)| {
                let pats =
                    t.get("index_patterns").and_then(|v| v.as_array()).cloned().unwrap_or_default();
                pats.iter()
                    .filter_map(|p| p.as_str())
                    .any(|p| p == index || wildcard_to_regex(p).is_match(index))
            })
            .map(|(n, t)| (t.get("order").and_then(|o| o.as_i64()).unwrap_or(0), n, t))
            .collect();
        matched.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
        // A composable template is not layered with the others: the one with
        // the highest priority makes the index and the rest do not, which is
        // what `_simulate_index` has always answered. Layering them the way
        // the older templates are layered meant a template that lost still
        // put its settings and its mappings into the index -- an index made
        // under a priority 9 template came out with the refresh interval and
        // the fields of the priority 1 template beside it, and the server
        // disagreed with its own simulation about what it had just made.
        let composable: Vec<_> =
            matched.iter().filter(|(_, _, t)| t.get("__composable").is_some()).cloned().collect();
        let matched = if composable.is_empty() {
            matched
        } else {
            // the last of them is the highest priority, ties by name
            composable.into_iter().rev().take(1).collect()
        };
        if matched.is_empty() {
            return body.clone();
        }
        let mut merged = serde_json::json!({});
        for (_, _, t) in matched {
            for key in ["settings", "mappings", "aliases"] {
                if let Some(v) = t.get(key) {
                    let Some(into) = merged.as_object_mut() else { continue };
                    let slot = into.entry(key).or_insert(serde_json::json!({}));
                    deep_merge(slot, v);
                }
            }
        }
        // the request itself always wins over a template
        deep_merge(&mut merged, body);
        merged
    }
}

/// The stream a `.ds-<stream>-NNNNNN` name would belong to, by its shape.
fn stream_of_backing_name(index: &str) -> Option<&str> {
    let rest = index.strip_prefix(".ds-")?;
    let (stream, generation) = rest.rsplit_once('-')?;
    (generation.len() == 6 && generation.bytes().all(|b| b.is_ascii_digit()) && !stream.is_empty())
        .then_some(stream)
}

impl Store {
    /// A backing index made from the data stream template its stream's name
    /// matches: the template's settings, mappings and aliases, the timestamp
    /// field mapped as a date, and `_data_stream_timestamp` switched on, as
    /// the reference makes one. `None` when no data stream template matches.
    fn apply_stream_template(&self, stream: &str, _index: &str, body: &Value) -> Option<Value> {
        let templates = self.templates.read();
        let (_, flat, composable) = templates
            .iter()
            .filter_map(|(name, t)| {
                let c = t.get("__composable")?;
                c.get("data_stream")?;
                let matches = c.get("index_patterns").and_then(|v| v.as_array()).is_some_and(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .any(|p| p == stream || glob_match(p, stream))
                });
                matches.then(|| (c.get("priority").and_then(|p| p.as_i64()).unwrap_or(0), name, t))
            })
            .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(a.1)))
            .map(|(p, _, t)| (p, t.clone(), t.get("__composable").cloned().unwrap_or_default()))?;
        let field = composable
            .pointer("/data_stream/timestamp_field/name")
            .and_then(|v| v.as_str())
            .unwrap_or("@timestamp")
            .to_string();
        let mut merged = serde_json::json!({});
        for key in ["settings", "mappings", "aliases"] {
            if let Some(v) = flat.get(key) {
                merged[key] = v.clone();
            }
        }
        if !merged.get("mappings").is_some_and(|m| m.is_object()) {
            merged["mappings"] = serde_json::json!({});
        }
        merged["mappings"]["_data_stream_timestamp"] = serde_json::json!({"enabled": true});
        if !merged["mappings"].get("properties").is_some_and(|p| p.is_object()) {
            merged["mappings"]["properties"] = serde_json::json!({});
        }
        let path = format!("/mappings/properties/{}", field.replace('.', "/properties/"));
        if merged.pointer(&path).is_none() {
            merged["mappings"]["properties"][&field] = serde_json::json!({"type": "date"});
        }
        deep_merge(&mut merged, body);
        Some(merged)
    }
}

impl Store {
    /// Keep a script under the name a request will ask for it by.
    pub fn remember_script(&self, id: &str, script: Value) {
        self.scripts.write().insert(id.to_string(), script);
    }

    /// The script stored under this name.
    pub fn stored_script(&self, id: &str) -> Option<Value> {
        self.scripts.read().get(id).cloned()
    }

    /// Forget a stored script.
    pub fn forget_script(&self, id: &str) -> bool {
        self.scripts.write().remove(id).is_some()
    }
}

impl Store {
    /// What the cluster manager publishes besides indices, so another node
    /// can take over with them: templates, component templates, pipelines
    /// and stored scripts -- and the security configuration, which is the
    /// cluster's as OpenSearch's security index is.
    pub fn customs(&self) -> Value {
        let mut customs = serde_json::json!({
            "templates": self.get_templates(),
            "components": self.get_components(),
            "pipelines": {"ingest": self.pipelines("ingest"), "search": self.pipelines("search")},
            "scripts": self.scripts.read().clone(),
            // A repository is the cluster's, as OpenSearch keeps it in the
            // cluster metadata, and so are the records of the snapshots in
            // it: registered on whichever node answered, they lived on that
            // node alone, and a new cluster manager knew of no repository at
            // all -- it had to be registered again before anything could be
            // listed or restored.
            "repositories": self.repositories.read().clone(),
            "snapshots": self.snapshots.read().clone(),
        });
        if let Some(security) = self.security.wire() {
            customs["security"] = security;
        }
        customs
    }

    /// Take the manager's customs as this node's own.
    pub fn replace_customs(&self, v: &Value) {
        let map = |v: Option<&Value>| -> HashMap<String, Value> {
            v.and_then(|o| o.as_object())
                .map(|o| o.iter().map(|(k, x)| (k.clone(), x.clone())).collect())
                .unwrap_or_default()
        };
        *self.templates.write() = map(v.get("templates"));
        *self.components.write() = map(v.get("components"));
        {
            let mut p = self.pipelines.write();
            p.insert("ingest".into(), map(v.pointer("/pipelines/ingest")));
            p.insert("search".into(), map(v.pointer("/pipelines/search")));
            let any = p.get("ingest").map(|m| !m.is_empty()).unwrap_or(false);
            self.any_ingest_pipeline.store(any, std::sync::atomic::Ordering::Relaxed);
        }
        *self.scripts.write() = map(v.get("scripts"));
        *self.repositories.write() = map(v.get("repositories"));
        {
            let mut snapshots = self.snapshots.write();
            *snapshots = v
                .get("snapshots")
                .and_then(|o| o.as_object())
                .map(|o| o.iter().map(|(repo, held)| (repo.clone(), map(Some(held)))).collect())
                .unwrap_or_default();
        }
    }
}

fn hex_of(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<String> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> =
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect();
    String::from_utf8(bytes?).ok()
}
