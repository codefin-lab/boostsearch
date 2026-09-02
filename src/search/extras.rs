//! What a query needs beyond the query: the clauses that are settled from a
//! document's own values once the candidates are known.

use super::*;

/// Does this cluster still allow the queries that cost the most to run?
pub(crate) fn expensive_allowed(store: &Store) -> bool {
    store
        .cluster_setting("search.allow_expensive_queries")
        .map(|v| v != json!("false") && v != json!(false))
        .unwrap_or(true)
}

/// Turn that question into the list of documents it is really about.
pub(crate) fn replace_routing_exists(node: &mut Value, ids: &[String]) {
    match node {
        Value::Object(o) => {
            if o.get("exists").and_then(|e| e.get("field")).and_then(|f| f.as_str())
                == Some("_routing")
            {
                o.remove("exists");
                o.insert("ids".into(), json!({"values": ids}));
                return;
            }
            for (_, v) in o.iter_mut() {
                replace_routing_exists(v, ids);
            }
        }
        Value::Array(a) => {
            for v in a {
                replace_routing_exists(v, ids);
            }
        }
        _ => {}
    }
}

pub(crate) fn scan_extras(node: &Value, out: &mut Extras) {
    match node {
        Value::Object(o) => {
            for (k, v) in o {
                match k.as_str() {
                    "geo_shape" | "geo_bounding_box" | "geo_distance" | "geo_polygon" => {
                        out.geo = true
                    }
                    "intervals" => out.intervals = true,
                    "distance_feature" => out.distance_feature = true,
                    "_name" => out.named = true,
                    "exists" => {
                        if v.get("field").and_then(|f| f.as_str()) == Some("_routing") {
                            out.routing_exists = true;
                        }
                    }
                    "nested" if v.get("inner_hits").is_some() => {
                        out.nested_inner_hits = true;
                    }
                    _ => {}
                }
                scan_extras(v, out);
            }
        }
        Value::Array(a) => {
            for v in a {
                scan_extras(v, out);
            }
        }
        _ => {}
    }
}

/// The `intervals` clause of a query: the field it reads and the rule it asks.
pub(crate) fn find_intervals(node: &Value) -> Option<(String, Value)> {
    match node {
        Value::Object(o) => {
            if let Some(spec) = o.get("intervals").and_then(|v| v.as_object()) {
                let (field, rule) = spec.iter().next()?;
                return Some((field.clone(), rule.clone()));
            }
            o.values().find_map(find_intervals)
        }
        Value::Array(a) => a.iter().find_map(find_intervals),
        _ => None,
    }
}

/// The `distance_feature` clause of a query, wherever it sits.
pub(crate) fn find_distance_feature(node: &Value) -> Option<&Value> {
    match node {
        Value::Object(o) => {
            if let Some(spec) = o.get("distance_feature") {
                return Some(spec);
            }
            o.values().find_map(find_distance_feature)
        }
        Value::Array(a) => a.iter().find_map(find_distance_feature),
        _ => None,
    }
}

/// How far apart two moments are, in whatever unit the values are counted in.
pub(crate) fn date_distance(origin: &Value, value: &Value) -> Option<f64> {
    let read = |v: &Value| -> Option<f64> {
        match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => crate::store::canonical_date(&json!(s))
                .and_then(|d| crate::store::parse_date_lenient(&d))
                .map(|d| d.unix_timestamp_nanos() as f64),
            _ => None,
        }
    };
    Some((read(origin)? - read(value)?).abs())
}

/// A length of time, written the way a pivot is: a count and a unit.
pub(crate) fn parse_time_amount(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    let (n, unit) = s.split_at(split);
    let n: f64 = n.parse().ok()?;
    Some(
        n * match unit {
            "nanos" => 1.0,
            "micros" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" | "H" => 3_600e9,
            "d" => 86_400e9,
            _ => return None,
        },
    )
}

pub(crate) fn settle_by_value(
    cands: &mut Vec<Cand>,
    searchers: &Searchers,
    body: &Value,
    extras: &Extras,
) {
    // A geo query asks where a point is. The query built for it only says the
    // field is there, so each candidate's own position is read and placed.
    if let Some((field, shape)) =
        extras.geo.then(|| body.get("query").and_then(find_geo_clause)).flatten()
    {
        let path = format!("/{}", field.replace('.', "/"));
        cands.retain(|c| {
            let (_, searcher, st) = &searchers[c.shard];
            let g = st.read();
            let Some((_, src)) = source_of(searcher, &g, c.addr) else { return true };
            let src = derived_copy(src, &g.mapping);
            let Some(here) = src.pointer(&path) else { return false };
            // a field may hold one point or several; a pair of numbers is one
            let points: Vec<&Value> = match here {
                Value::Array(a) if a.iter().all(|v| v.is_number()) => vec![here],
                Value::Array(a) => a.iter().collect(),
                other => vec![other],
            };
            points.iter().any(|p| point_within(&shape, p))
        });
    }
    // An `intervals` query asks where in a field the words are. The query
    // built for it matches wherever they merely occur, so the candidates are
    // read back and their text analysed again to see whether they really do.
    if let Some((field, rule)) =
        extras.intervals.then(|| body.get("query").and_then(find_intervals)).flatten()
    {
        let path = format!("/{}", field.replace('.', "/"));
        cands.retain(|c| {
            let (_, searcher, st) = &searchers[c.shard];
            let g = st.read();
            let Some((_, src)) = source_of(searcher, &g, c.addr) else { return true };
            let src = derived_copy(src, &g.mapping);
            let Some(text) = src.pointer(&path) else { return false };
            let text = match text {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let analyse = |t: &str| crate::query::analyze_text(&g.index, t, None);
            let tokens = analyse(&text);
            // another field of the same document, read with its own analyzer
            let elsewhere = |other: &str, words: &str| -> Option<(Vec<String>, Vec<String>)> {
                let held = src.pointer(&format!("/{}", other.replace('.', "/")))?;
                let held = match held {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let named = ["search_analyzer", "analyzer"]
                    .iter()
                    .find_map(|key| g.mapping.field_option(other, key))
                    .and_then(|v| v.as_str().map(|s| s.to_string()));
                let cut = |t: &str| crate::query::analyze_text(&g.index, t, named.as_deref());
                Some((cut(&held), cut(words)))
            };
            !crate::query::interval_spans(&tokens, &rule, &analyse, &elsewhere).is_empty()
        });
    }
    // `distance_feature` scores by how near a value is to an origin. The
    // candidates are known by now, and each one's value can simply be read.
    if let Some(spec) =
        extras.distance_feature.then(|| body.get("query").and_then(find_distance_feature)).flatten()
    {
        let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
        let path = format!("/{}", field.replace('.', "/"));
        let pivot = spec.get("pivot").and_then(|v| v.as_str()).unwrap_or("");
        let origin = spec.get("origin").cloned().unwrap_or(Value::Null);
        let geo = origin.is_array() || origin.as_str().map(|s| s.contains(',')).unwrap_or(false);
        for c in cands.iter_mut() {
            let (_, searcher, st) = &searchers[c.shard];
            let g = st.read();
            let Some((_, src)) = source_of(searcher, &g, c.addr) else { continue };
            let src = derived_copy(src, &g.mapping);
            let Some(value) = src.pointer(&path) else { continue };
            let distance = if geo {
                geo_distance_metres(&origin, value)
            } else {
                date_distance(&origin, value)
            };
            let pivot_size = if geo { parse_distance(pivot) } else { parse_time_amount(pivot) };
            match (distance, pivot_size) {
                (Some(d), Some(p)) if p > 0.0 => c.score = (p / (p + d)) as f32,
                _ => {}
            }
        }
    }
}

/// The source with its derived fields in, where the mapping has any.
fn derived_copy(src: Value, mapping: &crate::store::Mapping) -> Value {
    if mapping.derived_fields().is_empty() {
        src
    } else {
        crate::store::with_derived(&src, mapping)
    }
}
