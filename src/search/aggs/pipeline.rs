//! Aggregations that read the buckets other aggregations produced.

use crate::search::*;

/// Take those out of the request, remembering which aggregation each was under.
pub(crate) fn strip_bucket_pipelines(
    node: &mut Value,
    at: &mut Vec<String>,
    out: &mut Vec<(Vec<String>, String, Value)>,
) {
    let Some(o) = node.as_object_mut() else { return };
    let names: Vec<String> = o.keys().map(|k| k.to_string()).collect();
    for name in names {
        let is_bucket_pipeline = o
            .get(&name)
            .and_then(|d| d.as_object())
            .map(|d| d.keys().any(|k| BUCKET_PIPELINES.contains(&k.as_str())))
            .unwrap_or(false);
        if is_bucket_pipeline {
            if let Some(def) = o.remove(&name) {
                out.push((at.clone(), name, def));
            }
            continue;
        }
        let Some(def) = o.get_mut(&name) else { continue };
        let subs = if def.get("aggs").is_some() { "aggs" } else { "aggregations" };
        if def.get(subs).is_some() {
            at.push(name.clone());
            let mut inner = def[subs].clone();
            strip_bucket_pipelines(&mut inner, at, out);
            def[subs] = inner;
            at.pop();
        }
    }
    // an aggregation left with nothing under it should not carry an empty list
    let empty: Vec<String> = o
        .iter()
        .filter(|(_, d)| {
            ["aggs", "aggregations"].iter().any(|k| {
                d.get(*k).and_then(|v| v.as_object()).map(|v| v.is_empty()).unwrap_or(false)
            })
        })
        .map(|(k, _)| k.to_string())
        .collect();
    for name in empty {
        if let Some(d) = o.get_mut(&name).and_then(|d| d.as_object_mut()) {
            d.remove("aggs");
            d.remove("aggregations");
        }
    }
}

/// Add a running value to each bucket of the aggregation it was written under.
///
/// The aggregation may sit under others, and each of those has buckets of its
/// own, so the walk down is a walk across every bucket at each step.
pub(crate) fn apply_bucket_pipeline(aggs: &mut Value, at: &[String], name: &str, def: &Value) {
    let Some((parent, above)) = at.split_last() else { return };
    if !above.is_empty() {
        let step = &above[0];
        let Some(node) = aggs.get_mut(step) else { return };
        let rest: Vec<String> = above[1..].iter().chain(std::iter::once(parent)).cloned().collect();
        match node.get_mut("buckets") {
            Some(Value::Array(list)) => {
                for b in list.iter_mut() {
                    apply_bucket_pipeline(b, &rest, name, def);
                }
            }
            Some(Value::Object(named)) => {
                for (_, b) in named.iter_mut() {
                    apply_bucket_pipeline(b, &rest, name, def);
                }
            }
            _ => apply_bucket_pipeline(node, &rest, name, def),
        }
        return;
    }
    let Some(target) = aggs.get_mut(parent) else { return };
    let Some(buckets) = target.get_mut("buckets").and_then(|b| b.as_array_mut()) else { return };
    let kind = def
        .as_object()
        .and_then(|o| {
            o.keys().map(|k| k.to_string()).find(|k| BUCKET_PIPELINES.contains(&k.as_str()))
        })
        .unwrap_or_default();
    let path = def
        .pointer(&format!("/{kind}/buckets_path"))
        .and_then(|v| v.as_str())
        .unwrap_or("_count")
        .to_string();
    let read = |b: &Value| -> Option<f64> {
        if path == "_count" {
            return b.get("doc_count").and_then(|v| v.as_f64());
        }
        let mut node = b;
        for step in path.split(['.', '>']) {
            node = node.get(step)?;
        }
        node.get("value").and_then(|v| v.as_f64()).or_else(|| node.as_f64())
    };
    let mut running = 0.0f64;
    let mut previous: Option<f64> = None;
    for b in buckets.iter_mut() {
        let Some(v) = read(b) else { continue };
        match kind.as_str() {
            "cumulative_sum" => {
                running += v;
                b[name] = json!({"value": running});
            }
            "derivative" => {
                if let Some(prev) = previous {
                    b[name] = json!({"value": v - prev});
                }
                previous = Some(v);
            }
            _ => {}
        }
    }
}

pub(crate) fn is_pipeline_agg(def: &Value) -> bool {
    def.as_object()
        .map(|o| o.keys().any(|k| PIPELINES.contains(&k.as_str())))
        .unwrap_or(false)
}

pub(crate) fn run_pipeline_agg(aggs: &Value, def: &Value) -> std::result::Result<Value, Response> {
    let Some(o) = def.as_object() else { return Ok(Value::Null) };
    let mut kind = String::new();
    for k in o.keys() {
        if PIPELINES.contains(&k.as_str()) {
            kind = k.clone();
            break;
        }
    }
    if kind.is_empty() {
        return Ok(Value::Null);
    }
    let spec = o.get(&kind).cloned().unwrap_or(Value::Null);
    let path = spec.get("buckets_path").and_then(|v| v.as_str()).unwrap_or("");
    if let Some(complaint) = buckets_path_problem(aggs, path) {
        return Err(err(StatusCode::BAD_REQUEST, "illegal_argument_exception", complaint));
    }
    let values = resolve_buckets_path(aggs, path);
    if values.is_empty() {
        return Ok(json!({"value": Value::Null}));
    }
    let sum: f64 = values.iter().sum();
    let n = values.len() as f64;
    let value = match kind.as_str() {
        "avg_bucket" => sum / n,
        "sum_bucket" => sum,
        "min_bucket" => values.iter().copied().fold(f64::INFINITY, f64::min),
        "max_bucket" => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        "stats_bucket" => {
            return Ok(json!({
                "count": values.len(),
                "min": values.iter().copied().fold(f64::INFINITY, f64::min),
                "max": values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                "avg": sum / n,
                "sum": sum,
            }));
        }
        _ => return Ok(json!({"value": Value::Null})),
    };
    Ok(json!({"value": value}))
}

/// `histo.v` means: the metric `v` of every bucket of `histo`.
/// What a `buckets_path` ends at, if it is not a single number.
///
/// A pipeline sums, averages or picks from a list of numbers. A path that
/// stops at a bucketing aggregation, or at a metric with several values,
/// names no such number, and saying which is more useful than a zero.
pub(crate) fn buckets_path_problem(aggs: &Value, path: &str) -> Option<String> {
    let segs: Vec<&str> = path.split('>').flat_map(|s| s.split('.')).collect();
    let mut node = aggs;
    let mut last = "";
    let mut crossed = false;
    for (i, seg) in segs.iter().enumerate() {
        last = seg;
        node = node.get(seg)?;
        let leaf = i + 1 == segs.len();
        if let Some(buckets) = node.get("buckets") {
            if leaf {
                // the last step is a bucketing aggregation, not a value
                let kind = match buckets.as_array().and_then(|a| a.first()) {
                    Some(b) if b.get("key").map(|k| k.is_string()).unwrap_or(false) => "StringTerms",
                    Some(_) => "LongTerms",
                    None => "LongTerms",
                };
                return Some(format!(
                    "buckets_path must reference either a number value or a single value \
                     numeric metric aggregation, got: [{kind}] at aggregation [{seg}]"
                ));
            }
            // a path may step through one bucketing aggregation, reading the
            // rest inside each bucket; a second one is a list of lists, which
            // is no single number
            if crossed {
                return Some(format!(
                    "buckets_path must reference either a number value or a single value \
                     numeric metric aggregation, got: [Object[]] at aggregation [{seg}]"
                ));
            }
            crossed = true;
            node = buckets.as_array().and_then(|a| a.first())?;
        }
    }
    // a metric holding several values names none of them
    if node.get("value").is_none() {
        if let Some(values) = node.get("values") {
            let many = match values {
                Value::Object(o) => o.len() > 1,
                Value::Array(a) => a.len() > 1,
                _ => false,
            };
            if many {
                return Some(format!(
                    "buckets_path must reference either a number value or a single value \
                     numeric metric aggregation, but [{last}] contains multiple values. Please \
                     specify which to use."
                ));
            }
        } else if node.is_object() && node.get("doc_count").is_none() {
            return Some(format!(
                "buckets_path must reference either a number value or a single value numeric \
                 metric aggregation, got: [Object[]] at aggregation [{last}]"
            ));
        }
    }
    None
}

pub(crate) fn resolve_buckets_path(aggs: &Value, path: &str) -> Vec<f64> {
    let mut segs = path.split('>').flat_map(|s| s.split('.'));
    let Some(first) = segs.next() else { return Vec::new() };
    let rest: Vec<&str> = segs.collect();
    let Some(node) = aggs.get(first) else { return Vec::new() };
    let Some(buckets) = node.get("buckets").and_then(|b| b.as_array()) else {
        return Vec::new();
    };
    buckets
        .iter()
        // an empty bucket has no value to contribute, which is what the
        // default gap policy asks for
        .filter(|b| b.get("doc_count").and_then(|c| c.as_u64()).map(|c| c > 0).unwrap_or(true))
        .filter_map(|b| {
            let mut cur = b;
            for seg in &rest {
                cur = cur.get(seg)?;
            }
            cur.get("value").and_then(|v| v.as_f64()).or_else(|| cur.as_f64())
        })
        .collect()
}
