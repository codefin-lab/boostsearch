//! `composite`: one bucket per combination, walked in key order.

use super::*;
use crate::search::*;

/// A composite aggregation over `terms` sources.
///
/// The sources are run as nested `terms` aggregations and the resulting tree is
/// flattened into one bucket per combination, which is what a composite is. Key
/// order is ascending across the whole tuple, as the paging contract requires.
pub(crate) fn run_composite_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
    weighted: bool,
) -> std::result::Result<Value, Response> {
    let spec = def.get("composite").cloned().unwrap_or(json!({}));
    let size = spec.get("size").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
    let after = spec.get("after").cloned();
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();

    // `sources` is a list of single-key objects, each naming one source
    let Some(list) = spec.get("sources") else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "Required [sources]",
        ));
    };
    if list.as_array().map(|a| a.is_empty()).unwrap_or(true) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "Composite [sources] cannot be null or empty",
        ));
    }
    // a page of composite buckets is held in memory while it is built, so
    // there is a ceiling on how big a page may be asked for
    if size > 65_536 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "too_many_buckets_exception",
            format!(
                "Trying to create too many buckets. Must be less than or equal to: [65536] but \
                 was [{size}]."
            ),
        ));
    }
    let mut sources: Vec<CompSource> = Vec::new();
    for entry in list.as_array().into_iter().flatten() {
        let Some((name, body)) = entry.as_object().and_then(|o| o.iter().next()) else {
            continue;
        };
        let Some((kind, source)) = body.as_object().and_then(|o| o.iter().next()) else {
            continue;
        };
        let field = source.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
        let desc = source.get("order").and_then(|o| o.as_str()) == Some("desc");
        let order = if desc { "desc" } else { "asc" };
        let format = source.get("format").and_then(|f| f.as_str()).map(|s| s.to_string());
        let mut zone_ms = 0i64;
        let (node, date) = match kind.as_str() {
            "terms" => (
                json!({"terms": {"field": field, "size": 65_536, "order": {"_key": order}}}),
                false,
            ),
            "histogram" => {
                let interval = source.get("interval").and_then(|v| v.as_f64()).unwrap_or(1.0);
                (json!({"histogram": {"field": field, "interval": interval}}), false)
            }
            "date_histogram" => {
                // a date is bucketed on the same even grid a number is, once
                // the step and the shift are counted in the same milliseconds
                let step = source
                    .get("fixed_interval")
                    .or_else(|| source.get("calendar_interval"))
                    .or_else(|| source.get("interval"))
                    .and_then(|v| v.as_str())
                    .and_then(parse_offset)
                    .map(|d| d.whole_milliseconds() as f64)
                    .filter(|d| *d > 0.0);
                let Some(step) = step else {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        "[composite] only supports fixed-length date intervals",
                    ));
                };
                let offset = source
                    .get("offset")
                    .and_then(|v| v.as_str())
                    .and_then(parse_offset)
                    .map(|d| d.whole_milliseconds() as f64)
                    .unwrap_or(0.0);
                // a zone shifts where each day begins, which on this grid is
                // the same thing as an offset
                let zone = source
                    .get("time_zone")
                    .and_then(|v| v.as_str())
                    .and_then(|z| crate::tz::offset_at(z, 0))
                    .map(|secs| secs as f64 * 1000.0)
                    .unwrap_or(0.0);
                zone_ms = zone as i64;
                // a day in a zone begins where that zone's midnight falls in
                // UTC, which is as far the other way as the zone sits from it
                let shift = (offset - zone).rem_euclid(step);
                (json!({"histogram": {"field": field, "interval": step, "offset": shift}}), true)
            }
            other => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!("[composite] does not support [{other}] sources"),
                ));
            }
        };
        if sources.iter().any(|s| s.name == *name) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("Composite source names must be unique, found duplicates: [{name}]"),
            ));
        }
        sources.push(CompSource {
            name: name.clone(),
            node,
            date,
            format,
            desc,
            least_ns: None,
            zone_ms,
            ip: targets
                .iter()
                .filter_map(|n| store.get(n))
                .any(|st| st.read().mapping.type_of(&field) == Some("ip")),
            missing_bucket: source.get("missing_bucket").and_then(|v| v.as_bool()).unwrap_or(false),
            missing_last: source.get("missing_order").and_then(|v| v.as_str()) != Some("first"),
            field: field.clone(),
        });
    }
    if sources.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "Composite [sources] cannot be null or empty",
        ));
    }

    // A date source is bucketed here rather than by BoostCore: the column is
    // absent from any segment whose documents all lack the field, and a
    // histogram over a column that is only sometimes there answers for only
    // some of the segments. The span is known from the extremes, so the grid
    // can be walked and each step counted through the ordinary query path.
    let mut flat: Vec<Value> = Vec::new();
    if let Some(at) = sources.iter().position(|s| s.date) {
        let source = &sources[at];
        let field = source.node.pointer("/histogram/field").and_then(|f| f.as_str()).unwrap_or("");
        let step =
            source.node.pointer("/histogram/interval").and_then(|v| v.as_f64()).unwrap_or(1.0);
        let shift =
            source.node.pointer("/histogram/offset").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let base = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
        let probe = json!({
            "__min": {"min": {"field": field}},
            "__max": {"max": {"field": field}},
        });
        let (_, extremes) = filtered_count(store, targets, &base, &Some(probe))?;
        let read = |k: &str| -> Option<f64> { extremes.as_ref()?.get(k)?.get("value")?.as_f64() };
        let (Some(lo), Some(hi)) = (read("__min"), read("__max")) else {
            return Ok(json!({"buckets": []}));
        };
        // a bucket is named in milliseconds, and a date_nanos reads out in
        // nanoseconds
        let per: f64 = targets
            .iter()
            .filter_map(|n| store.get(n))
            .find_map(|st| match st.read().mapping.type_of(field) {
                Some("date_nanos") => Some(1e6),
                _ => None,
            })
            .unwrap_or(1.0);
        let (lo, hi) = (lo / per, hi / per);
        let first = ((lo - shift) / step).floor() * step + shift;
        // the rest of the sources are a composite of their own, run once
        // inside each step
        let rest: Vec<Value> = spec
            .get("sources")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter().enumerate().filter(|(i, _)| *i != at).map(|(_, v)| v.clone()).collect()
            })
            .unwrap_or_default();
        let mut cursor = first;
        let mut guard = 0;
        while cursor <= hi && guard < 100_000 {
            guard += 1;
            let next = cursor + step;
            let window = json!({"range": {field: {
                "gte": crate::store::format_millis(cursor as i64, "iso8601"),
                "lt": crate::store::format_millis(next as i64, "iso8601"),
                "format": "strict_date_optional_time",
            }}});
            let narrowed = combine(main_query, Some(window));
            if rest.is_empty() {
                let (count, sub) =
                    count_with_sub_aggs(store, targets, &narrowed, &sub_aggs, weighted)?;
                if count > 0 {
                    let mut b = json!({
                        "key": {source.name.clone(): cursor as i64},
                        "doc_count": count,
                    });
                    if let Some(Value::Object(o)) = sub {
                        for (k, v) in o {
                            b[k] = v;
                        }
                    }
                    flat.push(b);
                }
            } else {
                let mut inner = json!({"composite": {"sources": rest, "size": 65_536}});
                if let Some(sub) = sub_aggs.as_ref() {
                    inner["aggs"] = sub.clone();
                }
                let answer =
                    run_composite_agg(store, targets, &Some(narrowed.clone()), &inner, weighted)?;
                for mut b in answer["buckets"].as_array().cloned().unwrap_or_default() {
                    if let Some(key) = b.get_mut("key").and_then(|k| k.as_object_mut()) {
                        let mut with_date = serde_json::Map::new();
                        with_date.insert(source.name.clone(), json!(cursor as i64));
                        for (k, v) in key.iter() {
                            with_date.insert(k.clone(), v.clone());
                        }
                        *key = with_date;
                    }
                    flat.push(b);
                }
            }
            cursor = next;
        }
    } else {
        // nest the sources outermost-first; the innermost carries the sub-aggs
        let mut request = sub_aggs.clone().unwrap_or_else(|| json!({}));
        for (i, source) in sources.iter().enumerate().rev() {
            let mut node = source.node.clone();
            if request.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
                node["aggs"] = request;
            }
            request = json!({format!("__c{i}"): node});
        }
        if weighted {
            inject_doc_count_helpers(&mut request);
        }

        let query = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
        let (_, res) = filtered_count(store, targets, &query, &Some(request.clone()))?;
        let Some(mut res) = res else { return Ok(json!({"buckets": []})) };
        if weighted {
            apply_doc_counts(&mut res);
        }
        flatten_composite(&res, 0, &sources, &mut serde_json::Map::new(), &mut flat);
        // a source may ask for a bucket of the documents that have no value
        // for it at all, which no terms aggregation will ever produce
        for source in sources.iter().filter(|s| s.missing_bucket) {
            let absent = json!({"bool": {"must_not": [{"exists": {"field": source.field}}]}});
            let narrowed = combine(main_query, Some(absent));
            let (count, sub) = count_with_sub_aggs(store, targets, &narrowed, &sub_aggs, weighted)?;
            if count == 0 {
                continue;
            }
            let mut key = serde_json::Map::new();
            for other in &sources {
                key.insert(other.name.clone(), Value::Null);
            }
            let mut b = json!({"key": Value::Object(key), "doc_count": count});
            if let Some(Value::Object(o)) = sub {
                for (k, v) in o {
                    b[k] = v;
                }
            }
            flat.push(b);
        }
    }
    // whether a date bucket comes back counted in milliseconds or in
    // nanoseconds is not fixed, but the earliest value is known, so which unit
    // an answer is in can be read off rather than assumed
    for source in &sources {
        let Some(least) = source.least_ns.filter(|v| *v != 0.0) else { continue };
        let path = format!("/key/{}", source.name);
        for b in flat.iter_mut() {
            let Some(key) = b.pointer(&path).and_then(|v| v.as_f64()) else { continue };
            if key.abs() >= least.abs() / 1_000.0 {
                b["key"][source.name.clone()] = json!((key / 1e6) as i64);
            }
        }
    }
    flat.sort_by(|a, b| composite_key_order(a, b, &sources));

    if let Some(after) = after.as_ref().and_then(|a| a.as_object()) {
        // a marker for a date source is written the way its key is written,
        // which is a date rather than the number behind it
        let mut after = after.clone();
        for source in &sources {
            if !source.date {
                continue;
            }
            let Some(Value::String(text)) = after.get(&source.name) else { continue };
            let Some(ms) = crate::store::canonical_date(&json!(text))
                .and_then(|d| crate::store::parse_date_lenient(&d))
                .map(|d| (d.unix_timestamp_nanos() / 1_000_000) as i64 - source.zone_ms)
            else {
                continue;
            };
            after.insert(source.name.clone(), json!(ms));
        }
        let marker = json!({"key": Value::Object(after.clone())});
        flat.retain(|b| composite_key_order(b, &marker, &sources) == Ordering::Greater);
    }
    let _more = flat.len() > size;
    flat.truncate(size);
    // a source that says how it wants its key written gets it written that way,
    // which happens once the page is settled so ordering stays numeric
    for source in &sources {
        let Some(pattern) = source.format.as_deref() else { continue };
        if !source.date {
            continue;
        }
        for b in flat.iter_mut() {
            let Some(ms) = b.pointer(&format!("/key/{}", source.name)).and_then(|v| v.as_i64())
            else {
                continue;
            };
            if let Some(text) = crate::store::format_millis_at(ms, pattern, source.zone_ms) {
                b["key"][source.name.clone()] = json!(text);
            }
        }
    }
    let mut out = json!({"buckets": flat});
    // the marker for the next page is where this one ended, whether or not
    // there turns out to be one
    if let Some(last) = out["buckets"].as_array().and_then(|a| a.last()) {
        out["after_key"] = last["key"].clone();
    }
    Ok(out)
}

pub(crate) fn flatten_composite(
    node: &Value,
    depth: usize,
    sources: &[CompSource],
    key: &mut serde_json::Map<String, Value>,
    out: &mut Vec<Value>,
) {
    let Some(buckets) = node.pointer(&format!("/__c{depth}/buckets")).and_then(|b| b.as_array())
    else {
        return;
    };
    for b in buckets {
        // a histogram fills the gaps between its buckets; a composite has no
        // gaps to fill, so an empty one is not a bucket at all
        if b.get("doc_count").and_then(|c| c.as_u64()) == Some(0) {
            continue;
        }
        let mut raw = b.get("key").cloned().unwrap_or(Value::Null);
        if sources[depth].ip
            && let Some(text) = raw.as_str().and_then(crate::store::ip_from_canonical)
        {
            raw = json!(text);
        }
        if sources[depth].date {
            // a date key is a whole number of milliseconds, not a float
            if let Some(n) = raw.as_f64() {
                raw = json!(n as i64);
            }
        }
        key.insert(sources[depth].name.clone(), raw);
        if depth + 1 < sources.len() {
            flatten_composite(b, depth + 1, sources, key, out);
        } else {
            let mut bucket = json!({
                "key": Value::Object(key.clone()),
                "doc_count": b.get("doc_count").cloned().unwrap_or(json!(0)),
            });
            // anything else under the bucket is a sub-aggregation of the composite
            if let Some(o) = b.as_object() {
                for (k, v) in o {
                    // the key belongs to the composite, written its own way,
                    // and the bucketing agg's spelling of it is not wanted
                    if k != "key"
                        && k != "key_as_string"
                        && k != "doc_count"
                        && !k.starts_with("__c")
                    {
                        bucket[k] = v.clone();
                    }
                }
            }
            out.push(bucket);
        }
    }
    key.remove(&sources[depth].name);
}

pub(crate) fn composite_key_order(a: &Value, b: &Value, sources: &[CompSource]) -> Ordering {
    for source in sources {
        let name = &source.name;
        let (x, y) = (a.pointer(&format!("/key/{name}")), b.pointer(&format!("/key/{name}")));
        let missing = |v: Option<&Value>| matches!(v, None | Some(Value::Null));
        if missing(x) || missing(y) {
            let last = if source.missing_last { Ordering::Greater } else { Ordering::Less };
            let ord = match (missing(x), missing(y)) {
                (true, true) => Ordering::Equal,
                (true, false) => last,
                _ => last.reverse(),
            };
            if ord != Ordering::Equal {
                return ord;
            }
            continue;
        }
        let ord = match (x, y) {
            (Some(Value::Number(m)), Some(Value::Number(n))) => m
                .as_f64()
                .unwrap_or(0.0)
                .partial_cmp(&n.as_f64().unwrap_or(0.0))
                .unwrap_or(Ordering::Equal),
            (Some(Value::String(m)), Some(Value::String(n))) => m.cmp(n),
            (Some(m), Some(n)) => m.to_string().cmp(&n.to_string()),
            _ => Ordering::Equal,
        };
        let ord = if source.desc { ord.reverse() } else { ord };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}
