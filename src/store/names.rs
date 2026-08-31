//! Index names: how one is spelled on disk, matched by a pattern, or
//! written with a date in it.

use super::*;

pub fn id_fingerprint(id: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    // final mix so short ids spread across the whole 64-bit space
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h
}

/// A stable 22-character identifier derived from the index name, in the
/// alphabet the API uses for these.
pub fn index_uuid(name: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    let mut g: u64 = h ^ 0x9e37_79b9_7f4a_7c15;
    let mut out = String::with_capacity(22);
    for i in 0..22 {
        let src = if i % 2 == 0 { &mut h } else { &mut g };
        *src = src.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        out.push(ALPHABET[((*src >> 58) & 63) as usize] as char);
    }
    out
}

/// Put an alias definition into the form it is read back in.
///
/// `routing` is shorthand: it sets the routing used for indexing and the one
/// used for searching at once, and only those two are ever reported. Anything
/// else the caller wrote -- a filter, is_write_index -- is kept as it stands.
pub fn normalize_alias(def: &Value) -> Value {
    let Some(obj) = def.as_object() else { return serde_json::json!({}) };
    let mut out = obj.clone();
    if let Some(r) = out.remove("routing") {
        for key in ["index_routing", "search_routing"] {
            out.entry(key.to_string()).or_insert_with(|| r.clone());
        }
    }
    // a routing value is a string even when it was written as a number
    for key in ["index_routing", "search_routing"] {
        if let Some(v) = out.get(key) {
            if let Some(n) = v.as_i64() {
                out.insert(key.to_string(), Value::String(n.to_string()));
            } else if let Some(f) = v.as_f64() {
                out.insert(key.to_string(), Value::String(f.to_string()));
            }
        }
    }
    Value::Object(out)
}

/// Index names are not path-safe, so each one gets a stable encoded directory.
pub fn dir_name(index: &str) -> String {
    let mut out = String::new();
    for b in index.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02x}")),
        }
    }
    out
}

pub fn wildcard_to_regex(pat: &str) -> regex::Regex {
    let mut s = String::from("^");
    for c in pat.chars() {
        match c {
            '*' => s.push_str(".*"),
            '?' => s.push('.'),
            c => s.push_str(&regex::escape(&c.to_string())),
        }
    }
    s.push('$');
    regex::Regex::new(&s).unwrap_or_else(|_| regex::Regex::new("^$").unwrap())
}

/// `date_*` against a field name -- the only wildcard a template `match` uses.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let mut rest = name;
    let mut parts = pattern.split('*').peekable();
    let first = parts.next().unwrap_or("");
    if !rest.starts_with(first) {
        return false;
    }
    rest = &rest[first.len()..];
    if !pattern.contains('*') {
        return rest.is_empty();
    }
    while let Some(part) = parts.next() {
        if part.is_empty() {
            if parts.peek().is_none() {
                return true;
            }
            continue;
        }
        if parts.peek().is_none() {
            return rest.ends_with(part);
        }
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    true
}

/// `2019-12-15||/d`, `now-1d`, `now+1M/M`: an anchor followed by shifts and a
/// rounding, which is how OpenSearch writes a date relative to another.
/// Resolve a date-math index name into the name it stands for.
///
/// `<logstash-{now/M}>` names the index for the current month; the braces hold
/// a date expression and, after a pipe, how to write it.
pub fn resolve_date_math_name(name: &str) -> String {
    let Some(inner) = name.strip_prefix('<').and_then(|s| s.strip_suffix('>')) else {
        return name.to_string();
    };
    let mut out = String::new();
    let mut rest = inner;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let body = &rest[open + 1..open + close];
        rest = &rest[open + close + 1..];
        // the expression may name its own format after a pipe
        let (expr, fmt) = match body.split_once('|') {
            Some((e, f)) => (e, f),
            None => (body, "yyyy.MM.dd"),
        };
        match parse_date_math(expr.trim()) {
            Some((d, _)) => out.push_str(&format_with_pattern(d, fmt.trim())),
            None => out.push_str(body),
        }
    }
    out.push_str(rest);
    out
}
