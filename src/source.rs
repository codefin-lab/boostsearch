//! `_source` include/exclude filtering, with the wildcard patterns OpenSearch allows.

use serde_json::{Map, Value};

fn matches(pattern: &str, path: &str) -> bool {
    if pattern == "*" || pattern == path {
        return true;
    }
    if pattern.contains('*') {
        let re = crate::store::wildcard_to_regex(pattern);
        if re.is_match(path) {
            return true;
        }
    }
    // `include.field1` selects the leaf; `include` selects the whole subtree
    path.starts_with(&format!("{pattern}."))
}

/// True when the pattern could still match something deeper than `path`.
fn prefix_of(pattern: &str, path: &str) -> bool {
    if pattern.starts_with(&format!("{path}.")) {
        return true;
    }
    if pattern.contains('*') {
        let head: String = pattern.split('*').next().unwrap_or("").to_string();
        return head.starts_with(path) || path.starts_with(head.trim_end_matches('.'));
    }
    false
}

pub fn filter(src: &Value, includes: &[String], excludes: &[String]) -> Value {
    if includes.is_empty() && excludes.is_empty() {
        return src.clone();
    }
    walk(src, "", includes, excludes).unwrap_or(Value::Object(Map::new()))
}

fn walk(v: &Value, path: &str, inc: &[String], exc: &[String]) -> Option<Value> {
    if !path.is_empty() && exc.iter().any(|p| matches(p, path)) {
        return None;
    }
    let included = inc.is_empty() || (!path.is_empty() && inc.iter().any(|p| matches(p, path)));

    match v {
        Value::Object(o) => {
            let mut out = Map::new();
            for (k, child) in o {
                let child_path = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                // keep descending while an include pattern still points deeper
                let keep_going = included || inc.iter().any(|p| {
                    matches(p, &child_path) || prefix_of(p, &child_path)
                });
                if !keep_going {
                    continue;
                }
                if let Some(kept) = walk(child, &child_path, inc, exc) {
                    out.insert(k.clone(), kept);
                }
            }
            if out.is_empty() && !included {
                return None;
            }
            Some(Value::Object(out))
        }
        Value::Array(a) => {
            if !included {
                return None;
            }
            let kept: Vec<Value> =
                a.iter().filter_map(|x| walk(x, path, inc, exc)).collect();
            Some(Value::Array(kept))
        }
        leaf => {
            if included {
                Some(leaf.clone())
            } else {
                None
            }
        }
    }
}

/// `filter_path` prunes the response to the named paths. Supports `*` for one
/// level and `**` for any depth; a `-` prefix excludes instead.
pub fn filter_path(value: &Value, spec: &str) -> Value {
    let mut includes: Vec<Vec<String>> = Vec::new();
    let mut excludes: Vec<Vec<String>> = Vec::new();
    for part in spec.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let (list, pat) = match part.strip_prefix('-') {
            Some(rest) => (&mut excludes, rest),
            None => (&mut includes, part),
        };
        list.push(pat.split('.').map(|s| s.to_string()).collect());
    }
    if includes.is_empty() && excludes.is_empty() {
        return value.clone();
    }
    prune(value, &[], &includes, &excludes).unwrap_or(Value::Null)
}

fn seg_matches(pat: &str, seg: &str) -> bool {
    pat == "*" || pat == "**" || pat == seg || {
        pat.contains('*') && crate::store::wildcard_to_regex(pat).is_match(seg)
    }
}

/// How a pattern relates to a path: `(fully matched, could match deeper)`.
///
/// `**` consumes any number of segments, and a pattern that runs out while the
/// path continues means the whole subtree below it is selected.
fn pattern_state(pat: &[String], path: &[String]) -> (bool, bool) {
    if path.is_empty() {
        return (pat.is_empty(), !pat.is_empty());
    }
    if pat.is_empty() {
        return (true, false);
    }
    if pat[0] == "**" {
        let mut full = false;
        for k in 0..=path.len() {
            let (f, _) = pattern_state(&pat[1..], &path[k..]);
            full |= f;
        }
        return (full, true);
    }
    if seg_matches(&pat[0], &path[0]) {
        return pattern_state(&pat[1..], &path[1..]);
    }
    (false, false)
}

fn excluded(exc: &[Vec<String>], path: &[String]) -> bool {
    !path.is_empty() && exc.iter().any(|p| pattern_state(p, path).0)
}

fn prune(
    v: &Value,
    path: &[String],
    inc: &[Vec<String>],
    exc: &[Vec<String>],
) -> Option<Value> {
    if excluded(exc, path) {
        return None;
    }
    // the root is never "included" on its own -- otherwise every child would be
    // kept and an includes list would filter nothing
    let included =
        inc.is_empty() || (!path.is_empty() && inc.iter().any(|p| pattern_state(p, path).0));

    match v {
        Value::Object(o) => {
            if included && exc.is_empty() && !path.is_empty() {
                return Some(v.clone());
            }
            let mut out = serde_json::Map::new();
            for (k, child) in o {
                let mut cp = path.to_vec();
                cp.push(k.clone());
                if excluded(exc, &cp) {
                    continue;
                }
                let keep = included
                    || inc.iter().any(|p| {
                        let (m, d) = pattern_state(p, &cp);
                        m || d
                    });
                if !keep {
                    continue;
                }
                if let Some(kept) = prune(child, &cp, inc, exc) {
                    out.insert(k.clone(), kept);
                }
            }
            if out.is_empty() && !included && !path.is_empty() {
                return None;
            }
            Some(Value::Object(out))
        }
        // array indices are not part of a filter_path
        Value::Array(a) => {
            let kept: Vec<Value> = a.iter().filter_map(|x| prune(x, path, inc, exc)).collect();
            Some(Value::Array(kept))
        }
        leaf => {
            if included {
                Some(leaf.clone())
            } else {
                None
            }
        }
    }
}
