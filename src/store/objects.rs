//! What a node holds beside indices: scrolls, templates, repositories,
//! snapshots, pipelines and data streams.

use super::*;

impl Store {
    /// `size` is how many documents each batch returns; the cursor is placed
    /// after the batch the opening search already delivered.
    pub fn open_scroll(
        &self,
        expr: &str,
        body: &Value,
        size: usize,
        after: Option<Vec<Value>>,
        implicit_sort: bool,
    ) -> String {
        let n = self.scroll_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!("boostsearch-scroll-{n:016x}");
        self.scrolls.write().insert(
            id.clone(),
            ScrollState {
                expr: expr.to_string(),
                body: body.clone(),
                offset: size,
                size,
                pit: self.open_pit(expr, 0),
                after,
                implicit_sort,
            },
        );
        id
    }

    pub fn read_scroll(&self, id: &str) -> Option<ScrollState> {
        self.scrolls.read().get(id).cloned()
    }

    pub fn advance_scroll(&self, id: &str, by: usize, after: Option<Vec<Value>>) {
        if let Some(s) = self.scrolls.write().get_mut(id) {
            s.offset += by;
            if after.is_some() {
                s.after = after;
            }
        }
    }

    pub fn close_scroll(&self, id: &str) -> bool {
        self.scrolls.write().remove(id).is_some()
    }

    pub fn close_all_scrolls(&self) -> usize {
        let mut s = self.scrolls.write();
        let n = s.len();
        s.clear();
        n
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
        let templates = self.templates.read();
        let mut matched: Vec<(i64, &Value)> = templates
            .values()
            .filter(|t| {
                let pats =
                    t.get("index_patterns").and_then(|v| v.as_array()).cloned().unwrap_or_default();
                pats.iter()
                    .filter_map(|p| p.as_str())
                    .any(|p| p == index || wildcard_to_regex(p).is_match(index))
            })
            .map(|t| (t.get("order").and_then(|o| o.as_i64()).unwrap_or(0), t))
            .collect();
        matched.sort_by_key(|(o, _)| *o);
        if matched.is_empty() {
            return body.clone();
        }
        let mut merged = serde_json::json!({});
        for (_, t) in matched {
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

impl Store {
    /// The name the next finished task is reported under.
    pub fn next_task_id(&self) -> String {
        let n = self.task_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        format!("node-0:{n}")
    }

    /// Keep what a task answered, for the caller that comes back for it.
    pub fn remember_task(&self, name: &str, answer: Value) {
        self.tasks.write().insert(name.to_string(), answer);
    }

    /// What a task answered, if this node ran it.
    pub fn task_answer(&self, name: &str) -> Option<Value> {
        self.tasks.read().get(name).cloned()
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
    /// and stored scripts.
    pub fn customs(&self) -> Value {
        serde_json::json!({
            "templates": self.get_templates(),
            "components": self.get_components(),
            "pipelines": {"ingest": self.pipelines("ingest"), "search": self.pipelines("search")},
            "scripts": self.scripts.read().clone(),
        })
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
    }
}
