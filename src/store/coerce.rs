//! The shape a document takes on the way into the index, which is not the
//! shape it keeps in `_source`.

use super::*;

/// Round to the nearest value a sixteen-bit float can hold.
pub fn half_float(v: f64) -> f64 {
    let bits = (v as f32).to_bits();
    let sign = bits >> 31;
    let exp = ((bits >> 23) & 0xff) as i32 - 127;
    // outside what the exponent can name, the value is kept as it is
    if !(-14..=15).contains(&exp) {
        return v;
    }
    let mantissa = bits & 0x007f_ffff;
    // ten bits of mantissa, rounded to nearest with ties going even
    let shift = 13;
    let round = (mantissa + (1 << (shift - 1)) + ((mantissa >> shift) & 1)) >> shift;
    let (exp, round) = if round > 0x3ff { (exp + 1, round >> 1) } else { (exp, round) };
    let out = (sign << 31) | (((exp + 127) as u32) << 23) | (round << shift);
    f32::from_bits(out) as f64
}

/// A property named with dots is a property of the object those dots name.
///
/// `object1.red` is `red` inside `object1`, and mappings written either way
/// have to merge with each other, which they only do once both are nested.
pub fn expand_dotted_properties(node: &mut Value) {
    let Some(props) = node.get_mut("properties").and_then(|p| p.as_object_mut()) else {
        return;
    };
    let dotted: Vec<String> = props.keys().filter(|k| k.contains('.')).cloned().collect();
    for name in dotted {
        let Some(def) = props.remove(&name) else { continue };
        let parts: Vec<&str> = name.split('.').collect();
        let mut node = props
            .entry(parts[0].to_string())
            .or_insert_with(|| serde_json::json!({"properties": {}}));
        for part in &parts[1..parts.len() - 1] {
            if node.get("properties").is_none() {
                node["properties"] = serde_json::json!({});
            }
            node =
                entry_of(&mut node["properties"], part, || serde_json::json!({"properties": {}}));
        }
        if node.get("properties").is_none() {
            node["properties"] = serde_json::json!({});
        }
        node["properties"][parts[parts.len() - 1]] = def;
    }
    // an object written out in full may itself hold dotted names
    if let Some(props) = node.get_mut("properties").and_then(|p| p.as_object_mut()) {
        for (_, def) in props.iter_mut() {
            expand_dotted_properties(def);
        }
    }
}

/// Recursive object merge; `patch` wins on conflict.
pub fn deep_merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                match b.get_mut(k) {
                    Some(slot) if slot.is_object() && v.is_object() => deep_merge(slot, v),
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, p) => *b = p.clone(),
    }
}

/// Convert a JSON document into a BoostCore document with both views plus `_source`.
/// Build the BoostCore document. Takes the source by value so the JSON tree is
/// moved into the first view instead of deep-copied for both.
/// Apply a normalizer the way OpenSearch does at index time.
pub fn normalize(value: &Value, normalizer: &str) -> Option<Value> {
    let Some(s) = value.as_str() else {
        // a value that is not text -- a date is a number in the index -- has
        // nothing to normalise, but the multi-field still needs its copy
        return normalizer.is_empty().then(|| value.clone());
    };
    match normalizer {
        "" => Some(Value::String(s.to_string())),
        "lowercase" => Some(Value::String(s.to_lowercase())),
        "uppercase" => Some(Value::String(s.to_uppercase())),
        _ => None,
    }
}

/// Whether a value can be read as the type its mapping declares.
///
/// Only the types with a real parse step are checked; a string field takes
/// whatever it is given.
pub(crate) fn value_is_valid(v: &Value, ty: &str, format: Option<&str>) -> bool {
    match ty {
        "date" | "date_nanos" => {
            date_number(v, format, ty == "date_nanos").is_some() || v.is_number()
        }
        "ip" => v.as_str().map(|s| canonical_ip(s).is_some()).unwrap_or(false),
        "byte" | "short" | "integer" | "long" | "unsigned_long" | "float" | "half_float"
        | "double" | "scaled_float" => match v {
            Value::Number(_) => true,
            Value::String(s) => s.parse::<f64>().is_ok(),
            _ => false,
        },
        "boolean" => {
            matches!(v, Value::Bool(_)) || matches!(v.as_str(), Some("true") | Some("false"))
        }
        _ => true,
    }
}

/// Field values that cannot be read as their mapped type.
///
/// A field that says `ignore_malformed` has its bad values dropped and its name
/// recorded; one that does not makes the whole write fail, which is how a
/// field-level `false` overrides an index-wide `true`.
pub fn scan_malformed(
    source: &Value,
    mapping: &Mapping,
    index_default: bool,
) -> std::result::Result<Vec<String>, (String, String)> {
    let mut ignored = Vec::new();
    walk_malformed(source, &mut String::new(), mapping, index_default, &mut ignored)?;
    ignored.sort();
    ignored.dedup();
    Ok(ignored)
}

/// Drop a leaf the index is not going to hold.
pub fn remove_path(node: &mut Value, path: &str) {
    let Some((head, rest)) = path.split_once('.') else {
        if let Some(o) = node.as_object_mut() {
            o.remove(path);
        }
        return;
    };
    if let Some(child) = node.as_object_mut().and_then(|o| o.get_mut(head)) {
        remove_path(child, rest);
    }
}

/// Bring a value in line with the type its mapping declares.
///
/// A client may send `"800.0"` for a field mapped as a float; OpenSearch stores
/// a number there, and queries phrased with a number have to find it.
pub(crate) fn coerce_leaves(node: &mut Value, path: &mut String, mapping: &Mapping) {
    // whatever is under a flat_object keeps the spelling and the type it was
    // sent with; nothing below the object is a field of its own
    if mapping.type_of(path) == Some("flat_object") {
        return;
    }
    match node {
        Value::Object(obj) => {
            let base = path.len();
            for (k, v) in obj.iter_mut() {
                if base > 0 {
                    path.push('.');
                }
                path.push_str(k);
                coerce_leaves(v, path, mapping);
                path.truncate(base);
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                coerce_leaves(v, path, mapping);
            }
        }
        leaf => {
            let ty = mapping.type_of(path);
            if matches!(ty, Some("date") | Some("date_nanos")) {
                let fmt = mapping.field_option(path, "format");
                let fmt = fmt.as_ref().and_then(|v| v.as_str());
                // a date is a number in the index, the way OpenSearch stores
                // one; `_source` still says whatever the client sent
                if let Some(n) = date_number(leaf, fmt, ty == Some("date_nanos")) {
                    *leaf = Value::Number(n.into());
                }
            } else if let Some(c) = coerce_leaf(leaf, ty) {
                *leaf = c;
            }
            // A field the mapping calls a number holds that kind of number,
            // whatever the document wrote: 1 and 1.0 are the same value to
            // OpenSearch, but they are different column types here, and an
            // aggregation reading one column would not see the other.
            match ty {
                Some("float" | "half_float" | "double" | "scaled_float") => {
                    if let Some(n) = leaf.as_i64() {
                        if let Some(f) = serde_json::Number::from_f64(n as f64) {
                            *leaf = Value::Number(f);
                        }
                    }
                }
                Some("long" | "integer" | "short" | "byte") => {
                    if let Some(f) = leaf.as_f64().filter(|f| f.fract() == 0.0) {
                        if leaf.as_i64().is_none() {
                            *leaf = Value::Number((f as i64).into());
                        }
                    }
                }
                _ => {}
            }
            // a half_float holds sixteen bits, so the value it keeps is the
            // nearest one that fits -- 184.4 becomes 184.375, and a search
            // paging past that number has to see the same figure the index does
            if ty == Some("half_float") {
                if let Some(n) = leaf.as_f64() {
                    if let Some(q) = serde_json::Number::from_f64(half_float(n)) {
                        *leaf = Value::Number(q);
                    }
                }
            }
        }
    }
}

/// A range field written with only one end is open at the other, which a
/// comparison against a missing sub-field cannot express. The open side is
/// filled with the extreme its type allows, in the indexing view only.
pub(crate) fn fill_open_ranges(out: &mut Value, mapping: &Mapping) {
    let ranges: Vec<(String, String)> = mapping
        .types
        .iter()
        .filter(|(_, t)| t.ends_with("_range"))
        .map(|(p, t)| (p.clone(), t.clone()))
        .collect();
    for (path, ty) in ranges {
        let pointer = format!("/{}", path.replace('.', "/"));
        let dated = ty.starts_with("date");
        let Some(node) = out.pointer_mut(&pointer).and_then(|n| n.as_object_mut()) else {
            continue;
        };
        if dated {
            for key in ["gte", "gt", "lte", "lt"] {
                if let Some(v) = node.get(key) {
                    if let Some(n) = date_number(v, None, false) {
                        node.insert(key.into(), Value::Number(n.into()));
                    }
                }
            }
        }
        // comparisons run against `gte`/`lte`, so an exclusive endpoint is
        // moved one step inward rather than left in a form nothing reads
        let step = |v: &Value, forward: bool| -> Option<Value> {
            if dated {
                // a date is milliseconds here, so the next value along is the
                // next millisecond
                let n = v.as_i64().or_else(|| date_number(v, None, false))?;
                return Some(Value::from(if forward { n + 1 } else { n - 1 }));
            }
            // a whole-number range steps by one; a fractional one has no next
            // value to move to, so the bound is kept as written
            let n = v.as_i64()?;
            Some(Value::from(if forward { n + 1 } else { n - 1 }))
        };
        for (from, to, forward) in [("gt", "gte", true), ("lt", "lte", false)] {
            if node.contains_key(to) {
                continue;
            }
            let Some(v) = node.get(from).cloned() else { continue };
            let moved = step(&v, forward).unwrap_or(v);
            node.insert(to.into(), moved);
        }
        let has_lower = node.contains_key("gte");
        let has_upper = node.contains_key("lte");
        if !has_lower {
            node.insert(
                "gte".into(),
                if dated { Value::from(DATE_FLOOR) } else { serde_json::json!(f64::MIN) },
            );
        }
        if !has_upper {
            node.insert(
                "lte".into(),
                if dated { Value::from(DATE_CEIL) } else { serde_json::json!(f64::MAX) },
            );
        }
    }
}

/// A flat_object is queryable by its own name, which means every value beneath
/// it has to live somewhere addressable. They are gathered into one list
/// alongside, in the indexing view only.
pub(crate) fn gather_flat_objects(out: &mut Value, mapping: &Mapping) {
    let flats: Vec<String> = mapping
        .types
        .iter()
        .filter(|(_, t)| t.as_str() == "flat_object")
        .map(|(p, _)| p.clone())
        .collect();
    let Some(obj) = out.as_object_mut() else { return };
    for path in flats {
        let pointer = format!("/{}", path.replace('.', "/"));
        let Some(node) = obj.get(path.split('.').next().unwrap_or(&path)) else { continue };
        let root = Value::Object(obj.clone());
        let Some(node) = root.pointer(&pointer).or(Some(node)) else { continue };
        let mut values = Vec::new();
        collect_leaves(node, &mut values);
        if values.is_empty() {
            continue;
        }
        obj.insert(format!("{path}.{FLAT_VALUES}"), Value::Array(values));
    }
}

pub(crate) fn collect_leaves(node: &Value, out: &mut Vec<Value>) {
    match node {
        Value::Object(o) => o.values().for_each(|v| collect_leaves(v, out)),
        Value::Array(a) => a.iter().for_each(|v| collect_leaves(v, out)),
        Value::Null => {}
        leaf => out.push(leaf.clone()),
    }
}

/// The type OpenSearch infers for a value before any template is consulted.
pub(crate) fn json_mapping_type(v: &Value) -> &'static str {
    match v {
        Value::Object(_) => "object",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_f64() && n.as_i64().is_none() {
                "double"
            } else {
                "long"
            }
        }
        Value::String(s) => {
            if s.len() >= 10
                && s.as_bytes()[4] == b'-'
                && s.as_bytes()[7] == b'-'
                && parse_date_lenient(s).is_some()
            {
                "date"
            } else {
                "string"
            }
        }
        Value::Array(a) => a.first().map(json_mapping_type).unwrap_or("string"),
        Value::Null => "string",
    }
}

pub(crate) fn coerce_leaf(v: &Value, ty: Option<&str>) -> Option<Value> {
    if matches!(ty, Some("date") | Some("date_nanos")) {
        return canonical_date(v).map(Value::String);
    }
    let s = v.as_str()?;
    match ty? {
        // a token_count field holds how many tokens the text produced, not
        // the text itself
        "token_count" => Some(Value::from(token_count(s))),
        "byte" | "short" | "integer" | "long" | "unsigned_long" => {
            // an integer field takes the whole part of a decimal, and the
            // magnitudes unsigned_long reaches do not survive a trip via f64
            let whole = s.split_once('.').map(|(a, _)| a).unwrap_or(s);
            whole
                .parse::<i64>()
                .ok()
                .map(Value::from)
                .or_else(|| whole.parse::<u64>().ok().map(Value::from))
        }
        "float" | "half_float" | "double" | "scaled_float" => {
            s.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(Value::Number)
        }
        "ip" => canonical_ip(s).map(Value::String),
        "boolean" => match s {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        _ => None,
    }
}

/// Add the normalized copies a mapping's multi-fields ask for. These only go
/// into the index; `_source` is always what the client sent.
///
/// The copy is added as a dotted top-level key, which the JSON fields expand
/// into the same path a nested object would produce -- and unlike nesting, it
/// does not collide with the parent being a scalar.
pub fn expand_for_indexing(source: &Value, mapping: &Mapping) -> Value {
    let subs = mapping.normalized_subfields();
    let mut out = source.clone();
    coerce_leaves(&mut out, &mut String::new(), mapping);
    fill_open_ranges(&mut out, mapping);
    gather_flat_objects(&mut out, mapping);
    if subs.is_empty() {
        return out;
    }
    let source = &out.clone();
    let Some(obj) = out.as_object_mut() else { return out };
    for (parent, sub, normalizer) in subs {
        let Some(v) = source.pointer(&format!("/{}", parent.replace('.', "/"))).cloned() else {
            continue;
        };
        let normalized = match &v {
            Value::Array(items) => {
                let mapped: Vec<Value> =
                    items.iter().filter_map(|x| normalize(x, &normalizer)).collect();
                if mapped.is_empty() {
                    continue;
                }
                Value::Array(mapped)
            }
            other => match normalize(other, &normalizer) {
                Some(n) => n,
                None => continue,
            },
        };
        // a multi-field of a date counts in its own resolution: a date is
        // milliseconds and a date_nanos is nanoseconds, and the copy carries
        // the number the parent was coerced to
        let mut normalized = normalized;
        let step = match (mapping.type_of(&parent), mapping.type_of(&format!("{parent}.{sub}"))) {
            (Some("date"), Some("date_nanos")) => 1_000_000i64,
            (Some("date_nanos"), Some("date")) => -1_000_000,
            _ => 0,
        };
        if step != 0 {
            let rescale = |v: &mut Value| {
                if let Some(n) = v.as_i64() {
                    *v = Value::from(if step > 0 { n * step } else { n / -step });
                }
            };
            match &mut normalized {
                Value::Array(items) => items.iter_mut().for_each(rescale),
                other => rescale(other),
            }
        }
        obj.insert(format!("{parent}.{sub}"), normalized);
    }
    out
}

pub fn make_doc(fields: &Fields, id: &str, source: Value, raw: &str, seq: u64) -> TantivyDocument {
    let mut d = TantivyDocument::default();
    d.add_text(fields.id, id);
    d.add_text(fields.source, raw);
    d.add_u64(fields.seq, seq);
    if let Value::Object(obj) = source {
        let converted: BTreeMap<String, OwnedValue> =
            obj.into_iter().map(|(k, v)| (k, OwnedValue::from(v))).collect();
        d.add_object(fields.dynamic, converted.clone());
        d.add_object(fields.raw, converted);
    }
    d
}
