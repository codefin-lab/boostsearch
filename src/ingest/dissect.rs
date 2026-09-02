//! Dissect: cut a string by the literal delimiters between named holes.
//!
//! `%{a} %{b}` takes the text before the first space as `a` and the rest
//! as `b`. A hole may append to a field (`%{+a}`), skip what it matches
//! (`%{?x}` or `%{}`), name a key (`%{*k}`) whose value comes from another
//! hole (`%{&k}`), or swallow the repeated delimiter that follows it
//! (`%{a->}`).

use serde_json::Value;

use super::IngestError;

#[derive(Clone, Debug)]
struct Hole {
    name: String,
    append: Option<usize>,
    skip: bool,
    named_key: bool,
    named_value: bool,
    right_pad: bool,
    /// the literal text that follows this hole, up to the next one
    delimiter: String,
}

pub struct Dissect {
    prefix: String,
    holes: Vec<Hole>,
    append_separator: String,
    pattern: String,
}

impl Dissect {
    pub fn compile(pattern: &str, append_separator: &str) -> Result<Dissect, IngestError> {
        let bad = |reason: &str| {
            IngestError::illegal(format!("Unable to parse pattern: {pattern} Reason: {reason}"))
        };
        let mut holes = Vec::new();
        let mut prefix = String::new();
        let mut rest = pattern;
        let mut first = true;
        while let Some(at) = rest.find("%{") {
            let literal = &rest[..at];
            if first {
                prefix = literal.to_string();
                first = false;
            } else if let Some(last) = holes.last_mut() {
                let h: &mut Hole = last;
                h.delimiter = literal.to_string();
            }
            let after = &rest[at + 2..];
            let Some(end) = after.find('}') else {
                return Err(bad("the pattern has an unclosed key"));
            };
            let mut key = &after[..end];
            rest = &after[end + 1..];
            let mut hole = Hole {
                name: String::new(),
                append: None,
                skip: false,
                named_key: false,
                named_value: false,
                right_pad: false,
                delimiter: String::new(),
            };
            if let Some(k) = key.strip_suffix("->") {
                hole.right_pad = true;
                key = k;
            }
            if let Some(k) = key.strip_prefix('+') {
                // an order may follow: %{+a/2}
                match k.split_once('/') {
                    Some((name, order)) => {
                        hole.name = name.to_string();
                        hole.append =
                            Some(order.parse().map_err(|_| bad("append order is not a number"))?);
                    }
                    None => {
                        hole.name = k.to_string();
                        hole.append = Some(0);
                    }
                }
            } else if let Some(k) = key.strip_prefix('?') {
                hole.name = k.to_string();
                hole.skip = true;
            } else if let Some(k) = key.strip_prefix('*') {
                hole.name = k.to_string();
                hole.named_key = true;
            } else if let Some(k) = key.strip_prefix('&') {
                hole.name = k.to_string();
                hole.named_value = true;
            } else {
                hole.name = key.to_string();
                if key.is_empty() {
                    hole.skip = true;
                }
            }
            holes.push(hole);
        }
        if holes.is_empty() {
            return Err(bad("the pattern has no keys"));
        }
        if let Some(last) = holes.last_mut() {
            last.delimiter = rest.to_string();
        }
        // two holes with nothing between them cannot be told apart
        for pair in holes.windows(2) {
            if pair[0].delimiter.is_empty() {
                return Err(bad("the pattern has keys with no delimiter between them"));
            }
        }
        Ok(Dissect {
            prefix,
            holes,
            append_separator: append_separator.to_string(),
            pattern: pattern.to_string(),
        })
    }

    pub fn parse(&self, text: &str) -> Result<Vec<(String, Value)>, IngestError> {
        let miss = || {
            IngestError::illegal(format!(
                "Unable to find match for dissect pattern: {} against source: {text}",
                self.pattern
            ))
        };
        let Some(mut rest) = text.strip_prefix(self.prefix.as_str()) else { return Err(miss()) };
        let mut values: Vec<(String, String)> = Vec::new();
        for hole in &self.holes {
            let value = if hole.delimiter.is_empty() {
                let v = rest;
                rest = "";
                v
            } else {
                let Some(at) = rest.find(&hole.delimiter) else { return Err(miss()) };
                let v = &rest[..at];
                rest = &rest[at + hole.delimiter.len()..];
                if hole.right_pad {
                    while let Some(more) = rest.strip_prefix(hole.delimiter.as_str()) {
                        rest = more;
                    }
                }
                v
            };
            if !hole.skip {
                values.push((hole.name.clone(), value.to_string()));
            }
        }
        if !rest.is_empty() {
            return Err(miss());
        }
        // appends join in their order; named keys take their value from the
        // hole that shares the name
        let mut out: Vec<(String, Value)> = Vec::new();
        let mut appends: Vec<(String, Vec<(usize, String)>)> = Vec::new();
        let mut keys: Vec<(String, String)> = Vec::new();
        let mut named: Vec<(String, String)> = Vec::new();
        for (hole, (name, value)) in self.holes.iter().filter(|h| !h.skip).zip(values) {
            if let Some(order) = hole.append {
                match appends.iter_mut().find(|(n, _)| *n == name) {
                    Some((_, parts)) => parts.push((order, value)),
                    None => appends.push((name.clone(), vec![(order, value)])),
                }
            } else if hole.named_key {
                keys.push((name, value));
            } else if hole.named_value {
                named.push((name, value));
            } else {
                out.push((name, Value::String(value)));
            }
        }
        for (name, mut parts) in appends {
            parts.sort_by_key(|(o, _)| *o);
            let joined: Vec<String> = parts.into_iter().map(|(_, v)| v).collect();
            out.push((name, Value::String(joined.join(&self.append_separator))));
        }
        for (name, key) in keys {
            if let Some((_, value)) = named.iter().find(|(n, _)| *n == name) {
                out.push((key, Value::String(value.clone())));
            }
        }
        Ok(out)
    }
}
