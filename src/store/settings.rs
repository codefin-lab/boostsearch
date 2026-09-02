//! An index's settings, and what it has learned about its own fields.

use super::*;

impl IdxState {
    /// Settings echoed back by GET _settings, including the defaults the
    /// YAML suite asserts on.
    pub fn effective_settings(&self) -> Value {
        // what an index carries whether or not anyone asked for it: when it
        // was made, what made it, what it is called, and how it is replicated
        let mut idx = serde_json::json!({
            "number_of_shards": "1",
            "number_of_replicas": "1",
            "provided_name": self.name,
            "creation_date": self.created_ms.to_string(),
            "uuid": self.uuid,
            "version": {"created": "136407827"},
            "replication": {"type": "DOCUMENT"},
        });
        // settings arrive either nested under `index` or flat, and OpenSearch
        // always echoes the values back as strings
        fn put(idx: &mut Value, k: &str, v: &Value) {
            let k = k.trim_start_matches("index.");
            idx[k] = match v {
                Value::String(_) => v.clone(),
                Value::Null => return,
                other => Value::String(other.to_string()),
            };
        }
        if let Some(user) = self.settings.as_object() {
            for (k, v) in user {
                if k == "index" {
                    if let Some(nested) = v.as_object() {
                        for (k2, v2) in nested {
                            put(&mut idx, k2, v2);
                        }
                    }
                } else {
                    put(&mut idx, k, v);
                }
            }
        }
        serde_json::json!({ "index": idx })
    }

    /// Record the dynamic types a document contributes.
    ///
    /// Bulk loads send the same shape over and over, so remember which shapes
    /// have been walked and skip the walk for repeats.
    pub fn observe(&mut self, source: &Value) {
        // kinds are always tracked: two documents can share a shape and still
        // differ in value type, and a missed kind means missed hits
        let mut path = std::mem::take(&mut self.kind_path_buf);
        path.clear();
        observe_kinds(source, &mut path, &mut self.observed_kinds);
        self.kind_path_buf = path;
        if let Some(obj) = source.as_object() {
            let mut sig: u64 = 0xcbf2_9ce4_8422_2325;
            for k in obj.keys() {
                for b in k.as_bytes() {
                    sig ^= *b as u64;
                    sig = sig.wrapping_mul(0x1000_0000_01b3);
                }
                sig ^= 0xff;
            }
            if !self.seen_shapes.insert(sig) {
                return;
            }
        }
        self.observe_inner(source)
    }

    fn observe_inner(&mut self, source: &Value) {
        fn walk(v: &Value, prefix: &str, out: &mut HashMap<String, String>) {
            match v {
                Value::Object(o) => {
                    for (k, child) in o {
                        let path =
                            if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                        walk(child, &path, out);
                    }
                }
                Value::Array(a) => {
                    for x in a {
                        walk(x, prefix, out);
                    }
                }
                leaf if !prefix.is_empty() => {
                    let kind = match leaf {
                        Value::String(s) => {
                            if crate::query::parse_datetime(s).is_some() {
                                "date"
                            } else {
                                "text"
                            }
                        }
                        Value::Bool(_) => "boolean",
                        Value::Number(n) if n.is_f64() && n.as_i64().is_none() => "float",
                        Value::Number(_) => "long",
                        _ => return,
                    };
                    out.entry(prefix.to_string()).or_insert_with(|| kind.to_string());
                }
                _ => {}
            }
        }
        walk(source, "", &mut self.dynamic_types);
    }

    /// Every field path known for this index, explicit mappings taking priority.
    pub fn all_field_types(&self) -> Vec<(String, String)> {
        let mut out: HashMap<String, String> = self.dynamic_types.clone();
        for (k, v) in &self.mapping.types {
            out.insert(k.clone(), v.clone());
        }
        let mut v: Vec<(String, String)> = out.into_iter().collect();
        v.sort();
        v
    }

    /// Look a setting up without building the merged view.
    ///
    /// `effective_settings` allocates a fresh JSON object to fold the defaults
    /// in, which is the right shape for GET _settings but far too much work for
    /// a value read on every shard of every query.
    fn raw_setting(&self, key: &str) -> Option<&Value> {
        let settings = self.settings.as_object()?;
        if let Some(nested) = settings.get("index").and_then(|v| v.as_object()) {
            // a setting written flat keeps the `index.` prefix it arrived with,
            // even once it is filed under `index`
            if let Some(v) = nested
                .get(key)
                .or_else(|| nested.get(&format!("index.{key}")))
                .filter(|v| !v.is_null())
            {
                return Some(v);
            }
        }
        settings.get(key).or_else(|| settings.get(&format!("index.{key}"))).filter(|v| !v.is_null())
    }

    /// The moment now, written the way a timestamp is reported.
    pub fn now_iso() -> String {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i128)
            .unwrap_or(0);
        boostcore::time::OffsetDateTime::from_unix_timestamp_nanos(ms * 1_000_000)
            .map(|d| {
                format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
                    d.year(),
                    d.month() as u8,
                    d.day(),
                    d.hour(),
                    d.minute(),
                    d.second(),
                    d.millisecond(),
                )
            })
            .unwrap_or_default()
    }

    pub fn created_millis(&self) -> u64 {
        // a setting written by hand wins, since a restored index keeps the
        // date it was first made
        self.numeric_setting("creation_date").unwrap_or(self.created_ms)
    }

    /// The creation date as text, which is the other spelling `_cat` offers.
    pub fn created_string(&self) -> String {
        let ms = self.created_millis() as i128;
        boostcore::time::OffsetDateTime::from_unix_timestamp_nanos(ms * 1_000_000)
            .map(|d| {
                format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
                    d.year(),
                    d.month() as u8,
                    d.day(),
                    d.hour(),
                    d.minute(),
                    d.second(),
                    d.millisecond(),
                )
            })
            .unwrap_or_default()
    }

    pub fn numeric_setting(&self, key: &str) -> Option<u64> {
        match self.raw_setting(key)? {
            Value::String(s) => s.parse().ok(),
            Value::Number(n) => n.as_u64(),
            _ => None,
        }
    }

    /// How many shards the index reports. Read per shard of every query.
    pub fn shard_count(&self) -> u64 {
        self.numeric_setting("number_of_shards").unwrap_or(1)
    }

    /// Look a setting up by dotted name, accepting the flat or nested form.
    pub fn setting(&self, key: &str) -> Option<String> {
        // read off what was written rather than off the whole view an index
        // reports of itself: this is asked a few times for every document
        // written, and building that view each time was a tenth of the cost
        let written = |v: &Value| match v {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        };
        if let Some(v) = self.raw_setting(key).and_then(written) {
            return Some(v);
        }
        // a setting written as nested objects, under `index` or not
        let path = key.replace('.', "/");
        for pointer in [format!("/index/{path}"), format!("/{path}")] {
            if let Some(v) = self.settings.pointer(&pointer).and_then(written) {
                return Some(v);
            }
        }
        // what an index carries whether or not anyone wrote it
        match key {
            "number_of_shards" | "number_of_replicas" => Some("1".to_string()),
            "provided_name" => Some(self.name.clone()),
            "creation_date" => Some(self.created_ms.to_string()),
            "uuid" => Some(self.uuid.clone()),
            _ => None,
        }
    }

    /// `index.max_terms_count` caps how many terms a `terms` query may carry.
    /// `index.max_regex_length` caps how long a pattern a query may carry.
    pub fn max_regex_length(&self) -> usize {
        self.numeric_setting("max_regex_length").unwrap_or(1_000) as usize
    }

    pub fn max_terms_count(&self) -> usize {
        self.numeric_setting("max_terms_count").unwrap_or(65_536) as usize
    }
}
