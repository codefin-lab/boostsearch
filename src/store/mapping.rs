//! What an index says its fields are, and what it infers when it is not told.

use super::*;
use serde_json::json;

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
            copies: Vec::new(),
            flat_objects: Default::default(),
            has_percolator: false,
            ranges: Vec::new(),
            flats: Vec::new(),
            shingled: Vec::new(),
            nanos: Vec::new(),
            derived: Vec::new(),
            lenient: HashMap::new(),
            mapped_shapes: Default::default(),
        };
        m.remember_subfields();
        m
    }

    /// The fields whose values are written into other fields as well.
    ///
    /// A mapping may say `copy_to` on a field, naming one target or several.
    pub fn copies(&self) -> &[(String, Vec<String>)] {
        &self.copies
    }

    /// Whether any field of the mapping holds queries.
    pub fn has_percolator(&self) -> bool {
        self.has_percolator
    }

    fn collect_copies(&self) -> Vec<(String, Vec<String>)> {
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
    /// What a field's own `ignore_malformed` says, if it says anything.
    pub fn lenient_of(&self, field: &str) -> Option<bool> {
        self.lenient.get(field).copied()
    }

    pub fn learn_dynamic(&mut self, source: &Value) -> Vec<String> {
        // a document shaped like one already walked, whose every top-level
        // field is mapped, has nothing new to teach
        let mut sig: u64 = 0xcbf2_9ce4_8422_2325;
        let mut all_mapped = true;
        if let Some(obj) = source.as_object() {
            for k in obj.keys() {
                for b in k.as_bytes() {
                    sig ^= *b as u64;
                    sig = sig.wrapping_mul(0x1000_0000_01b3);
                }
                sig ^= 0xff;
                if !k.starts_with('_') && !self.types.contains_key(k) {
                    all_mapped = false;
                }
            }
        }
        if self.mapped_shapes.contains(&sig) {
            return Vec::new();
        }
        let before: std::collections::HashSet<String> = self
            .raw
            .get("properties")
            .and_then(|p| p.as_object())
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        let learned = self.learn_dynamic_walk(source);
        // only a shape that needed nothing is remembered: an object field may
        // hold new leaves the next time it appears
        if !learned
            && all_mapped
            && source
                .as_object()
                .map(|o| o.values().all(|v| !v.is_object() && !v.is_array()))
                .unwrap_or(false)
        {
            self.mapped_shapes.insert(sig);
        }
        if !learned {
            return Vec::new();
        }
        self.raw
            .get("properties")
            .and_then(|p| p.as_object())
            .map(|o| o.keys().filter(|k| !before.contains(*k)).cloned().collect())
            .unwrap_or_default()
    }
    fn learn_dynamic_walk(&mut self, source: &Value) -> bool {
        // an index told not to map what it is not told about keeps its
        // mapping as it was; the values are still stored and searched
        let dynamic = self.raw.get("dynamic").cloned().unwrap_or(Value::Bool(true));
        let off = matches!(
            dynamic.as_str().map(|s| s.to_ascii_lowercase()).as_deref(),
            Some("false" | "false_allow_templates" | "strict_allow_templates")
        ) || dynamic == Value::Bool(false);
        if off {
            return false;
        }
        let Some(obj) = source.as_object() else { return false };
        let mut learned: Vec<(String, Value)> = Vec::new();
        self.sniff_fields(obj, &mut String::new(), &mut learned);
        if learned.is_empty() {
            return false;
        }
        for (path, def) in learned {
            // a leaf under an object that holds no objects keeps its dotted
            // name as one key under that object
            if let Some(leaf) = def.get("__leaf__").and_then(|v| v.as_str()) {
                let parent = path[..path.len() - leaf.len() - 1].to_string();
                let def = def.get("def").cloned().unwrap_or(Value::Null);
                self.insert_flat_leaf(&parent, leaf, def);
                continue;
            }
            self.insert_path(&path, def);
        }
        // what the flat views know follows the raw mapping, once for the lot
        self.remember_subfields();
        true
    }

    /// Take fields back out of `properties`, keeping what was learned of
    /// their types: a derived object is not a property of the mapping.
    pub fn forget_properties(&mut self, names: &[String]) {
        if let Some(props) = self.raw.get_mut("properties").and_then(|p| p.as_object_mut()) {
            for n in names {
                props.remove(n);
            }
        }
    }

    /// One leaf written under an object by its whole dotted name.
    fn insert_flat_leaf(&mut self, parent: &str, leaf: &str, def: Value) {
        let Some(node) = self.raw.pointer_mut(&pointer_of(parent)) else { return };
        let props = entry_of(node, "properties", || json!({}));
        if let Some(o) = props.as_object_mut()
            && !o.contains_key(leaf)
        {
            o.insert(leaf.to_string(), def.clone());
        }
        if let Some(t) = def.get("type").and_then(|t| t.as_str()) {
            self.types.insert(format!("{parent}.{leaf}"), t.to_string());
        }
    }

    /// Every field the document holds that the mapping does not, with the
    /// mapping OpenSearch would give it: text with a keyword beside it for a
    /// string, a date where the string reads as one, long or float for a
    /// number, and an object for an object.
    fn sniff_fields(
        &self,
        node: &Map<String, Value>,
        path: &mut String,
        out: &mut Vec<(String, Value)>,
    ) {
        // runs on every document written, so the path is one buffer grown
        // and cut back rather than a string per node
        for (name, value) in node {
            if name.starts_with('_') {
                continue;
            }
            let base = path.len();
            if base > 0 {
                path.push('.');
            }
            path.push_str(name);
            let kind = self.types.get(path.as_str()).map(|s| s.as_str());
            // nothing under a flat_object is a field of its own, and a query
            // stored in a percolator field is a query, not a set of fields
            if matches!(kind, Some("flat_object" | "percolator")) {
                path.truncate(base);
                continue;
            }
            let known = kind.is_some();
            // a value that is an object, or a list of them, is looked into
            // even where the object itself is mapped: its fields may not be
            let inner: Option<&Map<String, Value>> = match value {
                Value::Object(o) => Some(o),
                Value::Array(items) => items.iter().find_map(|v| v.as_object()),
                _ => None,
            };
            if let Some(inner) = inner {
                // an object told to hold no objects of its own maps what is
                // under it as leaves named by their whole path
                if self.flat_objects.contains(path.as_str()) {
                    let here = path.clone();
                    let mut leaves: Vec<(String, Value)> = Vec::new();
                    let mut under = here.clone();
                    self.sniff_fields(inner, &mut under, &mut leaves);
                    for (leaf, def) in leaves {
                        if def.get("properties").is_some() {
                            continue;
                        }
                        let rest = leaf[here.len() + 1..].to_string();
                        out.push((format!("{here}.{rest}"), json!({"__leaf__": rest, "def": def})));
                    }
                    path.truncate(base);
                    continue;
                }
                if !known && self.raw.pointer(&pointer_of(path)).is_none() {
                    out.push((path.clone(), json!({"properties": {}})));
                }
                if kind != Some("nested") {
                    self.sniff_fields(inner, path, out);
                }
                path.truncate(base);
                continue;
            }
            if known || self.raw.pointer(&pointer_of(path)).is_some() {
                path.truncate(base);
                continue;
            }
            let leaf = match value {
                Value::Array(items) => match items.iter().find(|v| !v.is_null()) {
                    Some(first) => first,
                    None => {
                        path.truncate(base);
                        continue;
                    }
                },
                Value::Null => {
                    path.truncate(base);
                    continue;
                }
                other => other,
            };
            let def = match json_mapping_type(leaf) {
                "date" => Some(json!({"type": "date"})),
                "string" => Some(json!({
                    "type": "text",
                    "fields": {"keyword": {"type": "keyword", "ignore_above": 256}},
                })),
                "long" => Some(json!({"type": "long"})),
                // a floating point number nobody mapped is a float, not a double
                "double" => Some(json!({"type": "float"})),
                "boolean" => Some(json!({"type": "boolean"})),
                _ => None,
            };
            if let Some(def) = def {
                out.push((path.clone(), def));
            }
            path.truncate(base);
        }
    }

    /// Write one field's mapping in at its path, making the objects above it
    /// where they are not there yet.
    fn insert_path(&mut self, path: &str, def: Value) {
        if !self.raw.is_object() {
            self.raw = json!({});
        }
        let mut node = entry_of(&mut self.raw, "properties", || json!({}));
        let parts: Vec<&str> = path.split('.').collect();
        for part in &parts[..parts.len() - 1] {
            let field = entry_of(node, part, || json!({"properties": {}}));
            node = entry_of(field, "properties", || json!({}));
        }
        let leaf = parts[parts.len() - 1];
        if let Some(o) = node.as_object_mut()
            && !o.contains_key(leaf)
        {
            o.insert(leaf.to_string(), def.clone());
        }
        // what the flat view knows follows the raw mapping
        let mut one = Map::new();
        one.insert(leaf.to_string(), def);
        let parent = match parts.len() {
            1 => String::new(),
            n => parts[..n - 1].join("."),
        };
        flatten_props(&one, &parent, &mut self.types);
        if !parent.is_empty() {
            // every object above the leaf is a field too
            let mut walked = String::new();
            for part in &parts[..parts.len() - 1] {
                walked =
                    if walked.is_empty() { part.to_string() } else { format!("{walked}.{part}") };
                self.types.entry(walked.clone()).or_insert_with(|| "object".to_string());
            }
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

    pub fn normalized_subfields(&self) -> &[(String, String, String, String, String)] {
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
        self.copies = self.collect_copies();
        self.flat_objects = self
            .types
            .iter()
            .filter(|(_, kind)| *kind == "object")
            .map(|(path, _)| path.clone())
            .filter(|path| {
                self.field_option(path, "disable_objects")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            })
            .collect();
        self.has_percolator = self.types.values().any(|kind| kind == "percolator");
        let of = |keep: &dyn Fn(&str) -> bool| -> Vec<(String, String)> {
            let mut found: Vec<(String, String)> = self
                .types
                .iter()
                .filter(|(_, t)| keep(t))
                .map(|(p, t)| (p.clone(), t.clone()))
                .collect();
            found.sort();
            found
        };
        self.ranges = of(&|t| t.ends_with("_range"));
        self.flats = of(&|t| t == "flat_object").into_iter().map(|(p, _)| p).collect();
        self.shingled = of(&|t| t == "search_as_you_type").into_iter().map(|(p, _)| p).collect();
        self.nanos = of(&|t| t == "date_nanos").into_iter().map(|(p, _)| p).collect();
        // which fields say what happens to a malformed value
        self.lenient.clear();
        let mut lenient: HashMap<String, bool> = HashMap::new();
        if let Some(props) = self.raw.get("properties").and_then(|p| p.as_object()) {
            collect_lenient(props, "", &mut lenient);
        }
        self.lenient = lenient;
        // the shapes are learned again against the new mapping
        self.mapped_shapes.clear();
        // a derived field is typed like any other, so a query or an
        // aggregation reads it the way its type says
        self.derived = self
            .raw
            .get("derived")
            .and_then(|d| d.as_object())
            .map(|d| d.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        for (name, def) in self.derived.clone() {
            let kind = def.get("type").and_then(|t| t.as_str()).unwrap_or("keyword").to_string();
            self.types.insert(name.clone(), kind.clone());
            if kind == "object"
                && let Some(props) = def.get("properties").and_then(|p| p.as_object())
            {
                for (sub, sdef) in props {
                    // `keyword: keyword` is the short form of `{type: keyword}`
                    let t = sdef
                        .as_str()
                        .or_else(|| sdef.get("type").and_then(|t| t.as_str()))
                        .unwrap_or("keyword");
                    self.types.insert(format!("{name}.{sub}"), t.to_string());
                }
            }
        }
    }

    /// The fields a script makes from the source, as (name, definition).
    pub fn derived_fields(&self) -> &[(String, Value)] {
        &self.derived
    }

    /// Whether a path is a derived field, or lies under a derived object.
    pub fn is_derived(&self, path: &str) -> bool {
        self.derived.iter().any(|(n, _)| path == n || path.starts_with(&format!("{n}.")))
    }

    /// The range fields, as (path, type).
    pub fn range_fields(&self) -> &[(String, String)] {
        &self.ranges
    }

    /// The flat_object fields.
    pub fn flat_object_fields(&self) -> &[String] {
        &self.flats
    }

    /// The search_as_you_type fields.
    pub fn shingled_fields(&self) -> &[String] {
        &self.shingled
    }

    /// The date_nanos fields.
    pub fn nanos_fields(&self) -> &[String] {
        &self.nanos
    }

    /// Whether a path names a keyword sub-field with no normalizer, which is
    /// served from the raw view of its parent rather than from a copy.
    pub fn plain_keyword_sub(&self, field: &str) -> bool {
        let Some((parent, leaf)) = field.rsplit_once('.') else { return false };
        if self.type_of(field) != Some("keyword") {
            return false;
        }
        let Some(node) = self.raw.pointer(&pointer_of(parent)) else { return false };
        let Some(sub) = node.get("fields").and_then(|f| f.get(leaf)) else { return false };
        sub.get("normalizer").is_none() && sub.get("ignore_above").is_none()
    }

    /// The parent whose untouched view already holds this sub-field's values.
    ///
    /// `title.keyword` is how a text field's untouched view is addressed
    /// whether or not the mapping declares the sub-field; a sub-field under
    /// any other name -- `title.raw` is the common one -- is the same view
    /// when it is a plain keyword with nothing done to it on the way in.
    pub fn raw_view_parent<'a>(&self, field: &'a str) -> Option<&'a str> {
        if let Some(parent) = field.strip_suffix(".keyword")
            && !matches!(self.type_of(parent), Some("object" | "nested"))
        {
            return Some(parent);
        }
        if self.plain_keyword_sub(field) {
            return field.rsplit_once('.').map(|(parent, _)| parent);
        }
        None
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
            } else if key == "derived" {
                // derived fields are added to those already there, and one
                // named again is redefined
                let slot = entry_of(&mut self.raw, "derived", || serde_json::json!({}));
                if let (Some(existing), Some(incoming)) = (slot.as_object_mut(), val.as_object()) {
                    for (k, v) in incoming {
                        existing.insert(k.clone(), v.clone());
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
    out: &mut Vec<(String, String, String, String, String)>,
) {
    for (name, def) in props {
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        if let Some(subs) = def.get("fields").and_then(|f| f.as_object()) {
            for (sub, sdef) in subs {
                // a multi-field without a normalizer still needs its own copy
                // of the value; nothing else populates that path
                let n = sdef.get("normalizer").and_then(|v| v.as_str()).unwrap_or("");
                // a plain keyword beside a field holds what the raw view of
                // the field already holds, and is read from there -- unless it
                // refuses long values, in which case it holds only the short
                if n.is_empty()
                    && sdef.get("type").and_then(|t| t.as_str()) == Some("keyword")
                    && sdef.get("ignore_above").is_none()
                {
                    continue;
                }
                // the pointer and the full path are worked out here, once,
                // rather than for every document written
                let pointer = format!("/{}", path.replace('.', "/"));
                let full = format!("{path}.{sub}");
                out.push((path.clone(), sub.clone(), n.to_string(), pointer, full));
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

/// The JSON pointer to a field's own mapping entry.
fn pointer_of(path: &str) -> String {
    format!("/properties/{}", path.replace('.', "/properties/"))
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

/// Every field that says what a malformed value does, by path.
fn collect_lenient(props: &Map<String, Value>, prefix: &str, out: &mut HashMap<String, bool>) {
    for (name, def) in props {
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        if let Some(v) = def.get("ignore_malformed") {
            let b = match v {
                Value::Bool(b) => Some(*b),
                Value::String(s) => s.parse().ok(),
                _ => None,
            };
            if let Some(b) = b {
                out.insert(path.clone(), b);
            }
        }
        if let Some(sub) = def.get("properties").and_then(|p| p.as_object()) {
            collect_lenient(sub, &path, out);
        }
        if let Some(sub) = def.get("fields").and_then(|p| p.as_object()) {
            collect_lenient(sub, &path, out);
        }
    }
}
