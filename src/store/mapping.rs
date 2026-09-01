//! What an index says its fields are, and what it infers when it is not told.

use super::*;

impl Mapping {
    pub fn from_body(body: &Value) -> Mapping {
        let mut body = body.clone();
        expand_dotted_properties(&mut body);
        let mut types = HashMap::new();
        if let Some(props) = body.get("properties").and_then(|p| p.as_object()) {
            flatten_props(props, "", &mut types);
        }
        let mut m = Mapping {
            types,
            raw: body,
            subs: Vec::new(),
            aliases: HashMap::new(),
            formats: HashMap::new(),
        };
        m.remember_subfields();
        m
    }

    /// The fields whose values are written into other fields as well.
    ///
    /// A mapping may say `copy_to` on a field, naming one target or several.
    pub fn copies(&self) -> Vec<(String, Vec<String>)> {
        let mut out = Vec::new();
        for path in self.types.keys() {
            let Some(named) = self.field_option(path, "copy_to") else { continue };
            let targets: Vec<String> = match named {
                Value::String(one) => vec![one],
                Value::Array(items) => {
                    items.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()
                }
                _ => continue,
            };
            if !targets.is_empty() {
                out.push((path.clone(), targets));
            }
        }
        out.sort();
        out
    }

    /// Multi-fields declared with a normalizer, as (parent path, sub name).
    ///
    /// A normalizer transforms the value at index time rather than tokenising
    /// it, so the sub-field needs its own copy of the value in the index.
    /// Add the mappings a document's new fields earn under `dynamic_templates`.
    ///
    /// Returns the offending field name when the mapping is strict about
    /// fields no template claims.
    pub fn apply_dynamic_templates(&mut self, source: &Value) -> Result<(), String> {
        let dynamic =
            self.raw.get("dynamic").and_then(|v| v.as_str()).unwrap_or("true").to_string();
        let templates = self
            .raw
            .get("dynamic_templates")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if templates.is_empty() && !dynamic.starts_with("strict") {
            return Ok(());
        }
        let Some(obj) = source.as_object() else { return Ok(()) };
        for (name, value) in obj {
            if name.starts_with('_') || self.types.contains_key(name) {
                continue;
            }
            if self.raw.pointer(&format!("/properties/{name}")).is_some() {
                continue;
            }
            let kind = json_mapping_type(value);
            let mut matched = false;
            for t in &templates {
                let Some(spec) = t.as_object().and_then(|o| o.values().next()) else { continue };
                let pattern = spec.get("match").and_then(|v| v.as_str()).unwrap_or("*");
                if !glob_match(pattern, name) {
                    continue;
                }
                if let Some(mt) = spec.get("match_mapping_type").and_then(|v| v.as_str())
                    && mt != "*"
                    && mt != kind
                {
                    continue;
                }
                if let Some(m) = spec.get("mapping") {
                    self.insert_property(name, m.clone());
                }
                matched = true;
                break;
            }
            if !matched && dynamic.starts_with("strict") {
                return Err(name.clone());
            }
        }
        Ok(())
    }

    fn insert_property(&mut self, name: &str, def: Value) {
        if !self.raw.is_object() {
            self.raw = serde_json::json!({});
        }
        let props = entry_of(&mut self.raw, "properties", || serde_json::json!({}));
        if let Some(o) = props.as_object_mut() {
            o.insert(name.to_string(), def.clone());
            let mut one = Map::new();
            one.insert(name.to_string(), def);
            flatten_props(&one, "", &mut self.types);
        }
    }

    /// Note the fields a document maps dynamically.
    ///
    /// Only dates are inferred. Every other type is stored the same way
    /// whether the mapping named it or not, but a date needs its own column,
    /// and nothing downstream can build one without knowing the field is one.
    pub fn learn_dynamic(&mut self, source: &Value) {
        let mut found = Vec::new();
        Self::sniff_dates(source, &mut String::new(), &self.types, &mut found);
        for path in found {
            self.types.insert(path, "date".into());
        }
    }

    fn sniff_dates(
        node: &Value,
        path: &mut String,
        known: &HashMap<String, String>,
        out: &mut Vec<String>,
    ) {
        // nothing under a flat_object is a field of its own: its values keep
        // the spelling they were sent with, whatever they look like
        if known.get(path.as_str()).map(|t| t == "flat_object").unwrap_or(false) {
            return;
        }
        match node {
            Value::Object(o) => {
                let base = path.len();
                for (k, v) in o {
                    if k.starts_with('_') {
                        continue;
                    }
                    if base > 0 {
                        path.push('.');
                    }
                    path.push_str(k);
                    Self::sniff_dates(v, path, known, out);
                    path.truncate(base);
                }
            }
            Value::Array(a) => {
                for v in a {
                    Self::sniff_dates(v, path, known, out);
                }
            }
            Value::String(s) => {
                // a full calendar date, not a bare year that happens to parse
                let dated = s.len() >= 10
                    && s.as_bytes()[4] == b'-'
                    && s.as_bytes()[7] == b'-'
                    && parse_date_lenient(s).is_some();
                if dated && !known.contains_key(path.as_str()) {
                    out.push(path.clone());
                }
            }
            _ => {}
        }
    }

    /// A knob declared on one field's mapping entry.
    pub fn field_option(&self, field: &str, key: &str) -> Option<Value> {
        let mut node = self.raw.get("properties")?;
        let mut segs = field.split('.').peekable();
        while let Some(seg) = segs.next() {
            node = node.as_object()?.get(seg)?;
            if segs.peek().is_some() {
                node = node.get("properties").or_else(|| node.get("fields"))?;
            }
        }
        node.get(key).cloned()
    }

    /// Every path whose mapping names an analyzer, and the name it names.
    ///
    /// A `text` field says how it is cut; a `fields` subfield under it says
    /// how that copy is cut. Both are paths in the document as the index
    /// writes them, which is what the index wants to be told.
    pub fn analyzed_paths(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(props) = self.raw.get("properties").and_then(|p| p.as_object()) {
            collect_analyzers(props, "", &mut out);
        }
        out
    }

    /// The normalizer a field's mapping declares, if any.
    pub fn normalizer_of(&self, field: &str) -> Option<String> {
        let (parent, sub) = field.rsplit_once('.')?;
        let props = self.raw.get("properties")?.as_object()?;
        let mut node = props.get(parent.split('.').next()?)?;
        for seg in parent.split('.').skip(1) {
            node = node.get("properties")?.as_object()?.get(seg)?;
        }
        node.get("fields")?.get(sub)?.get("normalizer")?.as_str().map(|s| s.to_string())
    }

    pub fn normalized_subfields(&self) -> &[(String, String, String)] {
        &self.subs
    }

    /// Work out again what the mapping says, after it changed: the normalized
    /// multi-fields, the aliases, and the formats dates are written in.
    pub(crate) fn remember_subfields(&mut self) {
        self.subs.clear();
        self.aliases.clear();
        self.formats.clear();
        if let Some(props) = self.raw.get("properties").and_then(|p| p.as_object()) {
            collect_normalizers(props, "", &mut self.subs);
            collect_indirections(props, "", &mut self.aliases, &mut self.formats);
        }
    }

    /// The format a date path declares, if it declares one.
    pub fn date_format(&self, field: &str) -> Option<&str> {
        self.formats.get(field).map(|s| s.as_str())
    }

    /// Types the mapping treats as a single value rather than a container.
    pub fn is_leaf_type(&self, field: &str) -> bool {
        matches!(
            self.type_of(field),
            Some(t) if t.ends_with("_range") || t == "flat_object" || t == "object"
        )
    }

    pub fn type_of(&self, field: &str) -> Option<&str> {
        // the aliases are known ahead of a write, so the common path is one
        // lookup in a map rather than a walk of the mapping tree
        if !self.aliases.is_empty()
            && let Some(target) = self.aliases.get(field)
        {
            return self.types.get(target.as_str()).map(|s| s.as_str());
        }
        self.types.get(field).map(|s| s.as_str())
    }

    /// A field declared as an `alias` is another name for a field that is
    /// really there; this is the name behind it.
    pub fn target_of(&self, field: &str) -> Option<&str> {
        self.aliases.get(field).map(|s| s.as_str())
    }

    /// PUT _mapping is additive: new properties layer onto the old ones, and
    /// top-level knobs like `dynamic` are replaced.
    pub fn merge(&mut self, body: &Value) {
        if !self.raw.is_object() {
            self.raw = serde_json::json!({});
        }
        let mut body = body.clone();
        expand_dotted_properties(&mut body);
        let body = &body;
        let Some(incoming) = body.as_object() else { return };
        for (key, val) in incoming {
            if key == "properties" {
                if let Some(props) = val.as_object() {
                    flatten_props(props, "", &mut self.types);
                    let slot = entry_of(&mut self.raw, "properties", || serde_json::json!({}));
                    if let Some(existing) = slot.as_object_mut() {
                        for (k, v) in props {
                            // a dotted name is an object with one field in it,
                            // written short; the mapping holds the long form
                            match k.split_once('.') {
                                Some((head, rest)) => {
                                    let parent = existing
                                        .entry(head.to_string())
                                        .or_insert_with(|| serde_json::json!({}));
                                    let inner = parent.as_object_mut().map(|o| {
                                        o.entry("properties".to_string())
                                            .or_insert_with(|| serde_json::json!({}))
                                    });
                                    if let Some(inner) = inner.and_then(|i| i.as_object_mut()) {
                                        inner.insert(rest.to_string(), v.clone());
                                    }
                                }
                                None => {
                                    existing.insert(k.clone(), v.clone());
                                }
                            }
                        }
                    }
                }
            } else if let Some(o) = self.raw.as_object_mut() {
                o.insert(key.clone(), val.clone());
            }
        }
        self.remember_subfields();
    }
}

pub(crate) fn collect_normalizers(
    props: &Map<String, Value>,
    prefix: &str,
    out: &mut Vec<(String, String, String)>,
) {
    for (name, def) in props {
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        if let Some(subs) = def.get("fields").and_then(|f| f.as_object()) {
            for (sub, sdef) in subs {
                // a multi-field without a normalizer still needs its own copy
                // of the value; nothing else populates that path
                let n = sdef.get("normalizer").and_then(|v| v.as_str()).unwrap_or("");
                out.push((path.clone(), sub.clone(), n.to_string()));
            }
        }
        if let Some(inner) = def.get("properties").and_then(|p| p.as_object()) {
            collect_normalizers(inner, &path, out);
        }
    }
}

pub(crate) fn flatten_props(
    props: &Map<String, Value>,
    prefix: &str,
    out: &mut HashMap<String, String>,
) {
    for (name, def) in props {
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        if let Some(sub) = def.get("properties").and_then(|p| p.as_object()) {
            // the container is a field in its own right: an object, or a
            // nested one if it says so
            out.insert(
                path.clone(),
                def.get("type").and_then(|t| t.as_str()).unwrap_or("object").to_string(),
            );
            flatten_props(sub, &path, out);
            continue;
        }
        if let Some(t) = def.get("type").and_then(|t| t.as_str()) {
            out.insert(path.clone(), t.to_string());
        }
        // multi-fields: `title.keyword`
        if let Some(subs) = def.get("fields").and_then(|f| f.as_object()) {
            flatten_props(subs, &path, out);
        }
    }
}

/// Walk a mapping's properties, gathering the analyzer each path names.
fn collect_analyzers(
    props: &serde_json::Map<String, Value>,
    prefix: &str,
    out: &mut Vec<(String, String)>,
) {
    for (name, spec) in props {
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        if let Some(analyzer) = spec.get("analyzer").and_then(|a| a.as_str()) {
            out.push((path.clone(), analyzer.to_string()));
        }
        if let Some(sub) = spec.get("properties").and_then(|p| p.as_object()) {
            collect_analyzers(sub, &path, out);
        }
        if let Some(sub) = spec.get("fields").and_then(|p| p.as_object()) {
            collect_analyzers(sub, &path, out);
        }
    }
}

/// Walk a mapping's properties for the two things a write asks about at every
/// node: which paths are aliases, and which dates name a format.
fn collect_indirections(
    props: &serde_json::Map<String, Value>,
    prefix: &str,
    aliases: &mut HashMap<String, String>,
    formats: &mut HashMap<String, String>,
) {
    for (name, spec) in props {
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        match spec.get("type").and_then(|t| t.as_str()) {
            Some("alias") => {
                if let Some(target) = spec.get("path").and_then(|p| p.as_str()) {
                    aliases.insert(path.clone(), target.to_string());
                }
            }
            Some("date") | Some("date_nanos") => {
                if let Some(f) = spec.get("format").and_then(|f| f.as_str()) {
                    formats.insert(path.clone(), f.to_string());
                }
            }
            _ => {}
        }
        if let Some(sub) = spec.get("properties").and_then(|p| p.as_object()) {
            collect_indirections(sub, &path, aliases, formats);
        }
        if let Some(sub) = spec.get("fields").and_then(|p| p.as_object()) {
            collect_indirections(sub, &path, aliases, formats);
        }
    }
}
