//! Patterns, and the terms a JSON path puts them behind.

use super::*;

pub(crate) fn json_path_bytes(field: Field, path: &str) -> Vec<u8> {
    // With nothing appended yet, the value bytes are exactly
    // `<json path><JSON_END_OF_PATH>` -- which is what AutomatonWeight wants.
    Term::from_field_json_path(field, path, true).serialized_value_bytes().to_vec()
}

/// The automaton runs over the whole serialised term, not just the text, so a
/// pattern has to be anchored with `<json path>\0<type byte>` first.
pub(crate) fn json_term_prefix_regex(field: Field, path: &str) -> String {
    let mut t = Term::from_field_json_path(field, path, true);
    t.append_type_and_str("");
    t.serialized_value_bytes().iter().map(|b| format!("\\x{b:02x}")).collect()
}

pub(crate) fn regex_query(field: Field, path: &str, pattern: &str) -> Result<Box<dyn Query>> {
    let anchored = format!("{}{pattern}", json_term_prefix_regex(field, path));
    let re = Regex::new(&anchored).map_err(|e| anyhow!("bad regex `{pattern}`: {e}"))?;
    Ok(Box::new(JsonAutomatonQuery {
        field,
        regex: Arc::new(re),
        json_path_bytes: json_path_bytes(field, path),
    }))
}

pub fn wildcard_to_regex(pat: &str) -> String {
    let mut s = String::new();
    let mut chars = pat.chars();
    while let Some(c) = chars.next() {
        // a backslash makes the next character a literal, `*` and `?` included
        if c == '\\' {
            if let Some(next) = chars.next() {
                if next.is_alphanumeric() {
                    s.push(next);
                } else {
                    s.push('\\');
                    s.push(next);
                }
            }
            continue;
        }
        match c {
            '*' => s.push_str(".*"),
            '?' => s.push('.'),
            c if "[]{}()|+.\\^$".contains(c) => {
                s.push('\\');
                s.push(c);
            }
            c => s.push(c),
        }
    }
    s
}

/// Lower a pattern's literal letters, leaving escapes alone -- `\\W` is not
/// `\\w`.
pub(crate) fn lowercase_regex(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut escaped = false;
    for c in pattern.chars() {
        if escaped {
            out.push(c);
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            out.push(c);
            continue;
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// Widen every cased letter of a pattern into a two-way character class, so a
/// literal can be matched without regard to case.
pub(crate) fn case_insensitive_regex(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut escaped = false;
    let mut in_class = false;
    for c in pattern.chars() {
        if !escaped {
            if c == '[' {
                in_class = true;
            } else if c == ']' {
                in_class = false;
            }
        }
        // inside a character class an expansion would nest brackets
        if escaped || in_class || !c.is_alphabetic() {
            out.push(c);
            escaped = !escaped && c == '\\';
            continue;
        }
        let (lo, up): (String, String) = (c.to_lowercase().collect(), c.to_uppercase().collect());
        if lo == up {
            out.push(c);
        } else {
            out.push_str(&format!("[{lo}{up}]"));
        }
    }
    out
}

pub(crate) fn escape_regex(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "[]{}()|+*?.\\^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
