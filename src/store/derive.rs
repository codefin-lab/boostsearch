//! Fields a script makes from the source.
//!
//! A derived field is not in the document as it was sent: its script reads
//! `params._source` and emits what the field holds, and what it emits is
//! indexed under the field's name -- searched, aggregated and fetched like a
//! field written by hand, but never stored in `_source`.

use serde_json::Value;

use super::Mapping;

/// The values each derived field takes for one document, as (name, value).
pub fn derived_values(source: &Value, mapping: &Mapping) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for (name, def) in mapping.derived_fields() {
        let Some(script) = def.get("script") else { continue };
        let Ok(compiled) = crate::painless::contexts::Compiled::of(script, &|_| None) else {
            continue;
        };
        let mut runner =
            crate::painless::contexts::Runner::new(&compiled.params).with_source_param(source);
        if runner.run(&compiled.script).is_err() {
            continue;
        }
        let emitted: Vec<Value> = runner.emitted.borrow().iter().map(|v| v.to_json()).collect();
        let kind = def.get("type").and_then(|t| t.as_str()).unwrap_or("keyword");
        let mut values: Vec<Value> = emitted
            .into_iter()
            .filter_map(|v| match (kind, v) {
                // an object is emitted as the JSON text of it
                ("object", Value::String(text)) => serde_json::from_str(&text).ok(),
                // a date emitted as milliseconds is held the way a date is read
                ("date", Value::Number(n)) => n
                    .as_i64()
                    .and_then(|ms| super::format_millis(ms, "strict_date_optional_time"))
                    .map(Value::String),
                (_, Value::Null) => None,
                (_, other) => Some(other),
            })
            .collect();
        let value = match values.len() {
            0 => continue,
            1 => values.pop().unwrap(),
            _ => Value::Array(values),
        };
        out.push((name.clone(), value));
    }
    out
}

/// Write the derived fields into a document that is about to be indexed.
pub fn derive_into(indexed: &mut Value, source: &Value, mapping: &Mapping) {
    if mapping.derived_fields().is_empty() {
        return;
    }
    let made = derived_values(source, mapping);
    if let Some(o) = indexed.as_object_mut() {
        for (name, value) in made {
            o.insert(name, value);
        }
    }
}

/// The source with its derived fields written in, for reading them back.
pub fn with_derived(source: &Value, mapping: &Mapping) -> Value {
    let mut out = source.clone();
    derive_into(&mut out, source, mapping);
    out
}
