//! The aggregations that bucket by an interval a value falls in.

use super::*;

/// `date_range`: one bucket per span of time.
///
/// Each range becomes a filter on the field, so the ordinary query path
/// answers it. The bounds are reported in epoch milliseconds however they were
/// written, while the key keeps the caller's own spelling.
pub(crate) fn run_date_range_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("date_range").cloned().unwrap_or(json!({}));
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let keyed = spec.get("keyed").and_then(|v| v.as_bool()).unwrap_or(false);
    let missing = spec.get("missing").cloned();

    // the request may name its own format; otherwise the mapping's applies
    let mapped_format = targets.iter().filter_map(|n| store.get(n)).next().and_then(|st| {
        st.read()
            .mapping
            .field_option(&field, "format")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
    });
    let format =
        spec.get("format").and_then(|f| f.as_str()).map(|s| s.to_string()).or(mapped_format);

    // a bound is the number the index holds, and the date it stands for
    let millis = |v: &Value| crate::store::date_number(v, format.as_deref(), false);
    let iso = |v: &Value| {
        millis(v).and_then(|ms| crate::store::format_millis(ms, "strict_date_optional_time"))
    };
    // a bound is named in the key the way it is reported beside it, not the
    // way the request happened to spell it
    let shown = |v: &Option<Value>| match v {
        // a bound written as a date is named in the key the way it is
        // reported beside it; one written as a number is a number
        Some(Value::String(s)) => iso(&json!(s)).unwrap_or_else(|| s.clone()),
        Some(other) if !other.is_null() => other.to_string(),
        _ => "*".to_string(),
    };

    let mut buckets = Vec::new();
    let mut keyed_out = serde_json::Map::new();
    // AbstractRangeBuilder sorts the ranges it was given by where they start,
    // so the buckets come back in that order however the request listed them
    let mut asked: Vec<Value> = ranges_of(&spec);
    let edge = |range: &Value, key: &str, open: f64| -> f64 {
        range.get(key).filter(|v| !v.is_null()).and_then(millis).map(|ms| ms as f64).unwrap_or(open)
    };
    asked.sort_by(|a, b| {
        edge(a, "from", f64::NEG_INFINITY)
            .total_cmp(&edge(b, "from", f64::NEG_INFINITY))
            .then_with(|| edge(a, "to", f64::INFINITY).total_cmp(&edge(b, "to", f64::INFINITY)))
    });
    for range in &asked {
        let from = range.get("from").cloned().filter(|v| !v.is_null());
        let to = range.get("to").cloned().filter(|v| !v.is_null());
        let mut clause = serde_json::Map::new();
        if let Some(f) = from.as_ref().and_then(millis) {
            clause.insert("gte".into(), json!(f));
        }
        if let Some(t) = to.as_ref().and_then(millis) {
            clause.insert("lt".into(), json!(t));
        }
        // the bounds are already the numbers the index holds, whatever format
        // the field itself was written in
        if !clause.is_empty() {
            clause.insert("format".into(), json!("epoch_millis"));
        }
        let unbounded = clause.is_empty();
        // a document with no value stands in with what `missing` names, and
        // so belongs to whichever bucket that value falls in
        let missing_here = missing
            .as_ref()
            .and_then(millis)
            .map(|ms| {
                from.as_ref().and_then(millis).map(|f| ms >= f).unwrap_or(true)
                    && to.as_ref().and_then(millis).map(|t| ms < t).unwrap_or(true)
            })
            .unwrap_or(false);
        let filter = if unbounded {
            // documents with no value take part when a stand-in was named
            if missing.is_some() {
                json!({"match_all": {}})
            } else {
                json!({"exists": {"field": field}})
            }
        } else if missing_here {
            json!({"bool": {"should": [
                {"range": {field.clone(): Value::Object(clause)}},
                {"bool": {"must_not": [{"exists": {"field": field}}]}},
            ], "minimum_should_match": 1}})
        } else {
            json!({"range": {field.clone(): Value::Object(clause)}})
        };
        let combined = combine(main_query, Some(filter));
        let (count, sub) = count_with_sub_aggs(store, targets, &combined, &sub_aggs, false)?;

        let key = range
            .get("key")
            .and_then(|k| k.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{}-{}", shown(&from), shown(&to)));
        let mut b = json!({"key": key.clone(), "doc_count": count});
        if let Some(f) = from.as_ref()
            && let Some(ms) = millis(f)
        {
            b["from"] = json!(ms);
            if let Some(s) = iso(f) {
                b["from_as_string"] = json!(s);
            }
        }
        if let Some(t) = to.as_ref()
            && let Some(ms) = millis(t)
        {
            b["to"] = json!(ms);
            if let Some(s) = iso(t) {
                b["to_as_string"] = json!(s);
            }
        }
        if let Some(Value::Object(o)) = sub {
            for (k, v) in o {
                b[k] = v;
            }
        }
        if keyed {
            keyed_out.insert(key, b);
        } else {
            buckets.push(b);
        }
    }
    if keyed {
        return Ok(json!({"buckets": Value::Object(keyed_out)}));
    }
    Ok(json!({"buckets": buckets}))
}

/// A numeric histogram over a range field.
///
/// A range document has no single value to fall into one bucket; it covers a
/// span, and belongs to every bucket that span touches. So each bucket is
/// counted on its own, by asking which stored ranges overlap it, rather than
/// by reading a column of values the field does not have.
pub(crate) fn run_range_field_histogram(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("histogram").cloned().unwrap_or_else(|| json!({}));
    let Some(field) = spec.get("field").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return Ok(json!({"buckets": []}));
    };
    let interval = spec.get("interval").and_then(|v| v.as_f64()).filter(|i| *i > 0.0);
    let Some(interval) = interval else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "[interval] must be >0 for histogram aggregation",
        ));
    };
    let min_doc_count = spec.get("min_doc_count").and_then(|v| v.as_u64()).unwrap_or(0);
    let base = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
    let bounds = spec.get("hard_bounds").or_else(|| spec.get("extended_bounds"));
    let bound = |k: &str| bounds.and_then(|b| b.get(k)).and_then(|v| v.as_f64());

    // without bounds the span is the widest the stored endpoints reach
    let (lo, hi) = match (bound("min"), bound("max")) {
        (Some(a), Some(b)) => (a, b),
        (a, b) => {
            let probe = json!({
                "__min": {"min": {"field": format!("{field}.gte")}},
                "__max": {"max": {"field": format!("{field}.lte")}},
            });
            let (_, extremes) = filtered_count(store, targets, &base, &Some(probe))?;
            let read =
                |k: &str| -> Option<f64> { extremes.as_ref()?.get(k)?.get("value")?.as_f64() };
            match (a.or_else(|| read("__min")), b.or_else(|| read("__max"))) {
                (Some(x), Some(y)) => (x, y),
                _ => return Ok(json!({"buckets": []})),
            }
        }
    };
    if !lo.is_finite() || !hi.is_finite() || hi < lo {
        return Ok(json!({"buckets": []}));
    }
    // buckets start on multiples of the interval, as they do for a plain field
    let first = (lo / interval).floor() * interval;
    let steps = (((hi - first) / interval).floor() as i64).clamp(0, 65_536);
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();

    let mut buckets = Vec::new();
    for i in 0..=steps {
        let key = first + i as f64 * interval;
        // a stored range overlaps this bucket when it starts before the
        // bucket ends and ends at or after the bucket starts
        let overlap = json!({"bool": {"filter": [
            {"range": {format!("{field}.gte"): {"lt": key + interval}}},
            {"range": {format!("{field}.lte"): {"gte": key}}},
            base.clone(),
        ]}});
        let (count, sub) = count_with_sub_aggs(store, targets, &overlap, &sub_aggs, false)?;
        if count < min_doc_count {
            continue;
        }
        let mut b = json!({
            "key": if key.fract() == 0.0 { json!(key as i64) } else { json!(key) },
            "doc_count": count,
        });
        if let (Some(sub), Some(o)) = (sub, b.as_object_mut())
            && let Some(entries) = sub.as_object()
        {
            for (k, v) in entries {
                o.insert(k.clone(), v.clone());
            }
        }
        buckets.push(b);
    }
    Ok(json!({"buckets": buckets}))
}

/// The ranges a request names: a list of them, or one written on its own,
/// read as a list of one.
pub(crate) fn ranges_of(spec: &Value) -> Vec<Value> {
    match spec.get("ranges") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Object(one)) => vec![Value::Object(one.clone())],
        _ => Vec::new(),
    }
}

pub(crate) fn run_ip_range_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("ip_range").cloned().unwrap_or(json!({}));
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let keyed = spec.get("keyed").and_then(|v| v.as_bool()).unwrap_or(false);

    let mut buckets = Vec::new();
    let mut keyed_out = serde_json::Map::new();
    for range in ranges_of(&spec).iter() {
        // a mask names the same span as the addresses at its edges
        let (from, to) = match range.get("mask").and_then(|m| m.as_str()) {
            Some(mask) => match crate::store::cidr_bounds(mask) {
                Some((lo, hi)) => (Some(json!(lo)), Some(json!(hi))),
                None => (None, None),
            },
            None => (range.get("from").cloned(), range.get("to").cloned()),
        };
        let mut clause = serde_json::Map::new();
        if let Some(f) = from.as_ref().filter(|v| !v.is_null()) {
            clause.insert("gte".into(), f.clone());
        }
        if let Some(t) = to.as_ref().filter(|v| !v.is_null()) {
            clause.insert("lt".into(), t.clone());
        }
        let filter = if clause.is_empty() {
            json!({"exists": {"field": field}})
        } else {
            json!({"range": {field.clone(): Value::Object(clause)}})
        };
        let combined = combine(main_query, Some(filter));
        let (count, sub) = count_with_sub_aggs(store, targets, &combined, &sub_aggs, false)?;

        let text = |v: &Option<Value>| match v {
            Some(Value::String(s)) => s.clone(),
            Some(other) if !other.is_null() => other.to_string(),
            _ => "*".to_string(),
        };
        let key = range
            .get("key")
            .and_then(|k| k.as_str())
            .map(|s| s.to_string())
            .or_else(|| range.get("mask").and_then(|m| m.as_str()).map(|s| s.to_string()))
            .unwrap_or_else(|| format!("{}-{}", text(&from), text(&to)));
        let mut b = json!({"key": key.clone(), "doc_count": count});
        if let Some(f) = from.as_ref().filter(|v| !v.is_null()) {
            b["from"] = f.clone();
        }
        if let Some(t) = to.as_ref().filter(|v| !v.is_null()) {
            b["to"] = t.clone();
        }
        if let Some(Value::Object(o)) = sub {
            for (k, v) in o {
                b[k] = v;
            }
        }
        if keyed {
            keyed_out.insert(key, b);
        } else {
            buckets.push(b);
        }
    }
    if keyed {
        return Ok(json!({"buckets": Value::Object(keyed_out)}));
    }
    Ok(json!({"buckets": buckets}))
}

/// `variable_width_histogram`: buckets whose edges follow the data.
///
/// The values are sorted and cut at the widest gaps, which puts the boundaries
/// where the data is already sparse. Each bucket is keyed by the mean of what
/// it holds.
pub(crate) fn run_variable_width_histogram(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("variable_width_histogram").cloned().unwrap_or(json!({}));
    let want = spec.get("buckets").and_then(|v| v.as_u64()).unwrap_or(10).max(1) as usize;
    let (field, missing) = agg_field_and_missing(&spec);
    let query = combine(main_query, None);
    let mut values = collect_field_values(store, targets, &query, &field, missing)?;
    if values.is_empty() {
        return Ok(json!({"buckets": []}));
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));

    // cut where the data is sparsest: the widest gaps between neighbours
    let mut gaps: Vec<(f64, usize)> =
        (1..values.len()).map(|i| (values[i] - values[i - 1], i)).collect();
    gaps.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
    let mut cuts: Vec<usize> =
        gaps.into_iter().take(want.saturating_sub(1)).map(|(_, i)| i).collect();
    cuts.sort_unstable();

    let mut buckets = Vec::new();
    let mut start = 0usize;
    for end in cuts.into_iter().chain(std::iter::once(values.len())) {
        let slice = &values[start..end];
        if slice.is_empty() {
            continue;
        }
        let sum: f64 = slice.iter().sum();
        buckets.push(json!({
            "min": slice[0],
            "key": sum / slice.len() as f64,
            "max": slice[slice.len() - 1],
            "doc_count": slice.len(),
        }));
        start = end;
    }
    Ok(json!({"buckets": buckets}))
}

/// `auto_date_histogram`: pick the smallest rounding that keeps the bucket
/// count within the target, then bucket by it.
///
/// The choice is made from the span the data actually covers rather than by
/// building each candidate histogram: at one-second resolution a week-long
/// span is over half a million buckets, which is a lot of searching to do only
/// to discard it.
pub(crate) fn run_auto_date_histogram(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("auto_date_histogram").cloned().unwrap_or(json!({}));
    let want = spec.get("buckets").and_then(|v| v.as_u64()).unwrap_or(10).max(1);
    let field = spec.get("field").cloned().unwrap_or(Value::Null);
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();

    let base = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
    let probe = json!({
        "__min": {"min": {"field": field}},
        "__max": {"max": {"field": field}},
    });
    let (_, extremes) = filtered_count(store, targets, &base, &Some(probe))?;
    let read = |k: &str| -> Option<f64> { extremes.as_ref()?.get(k)?.get("value")?.as_f64() };
    let (Some(lo), Some(hi)) = (read("__min"), read("__max")) else {
        return Ok(json!({"buckets": [], "interval": "1s"}));
    };
    // a date is a number in the index: milliseconds, or nanoseconds for a
    // date_nanos
    let per_ns: f64 = field
        .as_str()
        .and_then(|f| {
            targets.iter().filter_map(|n| store.get(n)).find_map(|st| {
                match st.read().mapping.type_of(f) {
                    Some("date_nanos") => Some(1.0),
                    Some(t) if t.starts_with("date") => Some(1_000_000.0),
                    _ => None,
                }
            })
        })
        .unwrap_or(1.0);
    let span_ns = ((hi - lo) * per_ns).max(0.0);

    // label, the unit the histogram steps by, and roughly how long it is
    const NS: f64 = 1e9;
    // the steps OpenSearch rounds to: fixed lengths below a day, calendar
    // units from a day up
    const STEPS: &[(&str, &str, f64)] = &[
        ("1s", "1s", NS),
        ("5s", "5s", 5.0 * NS),
        ("10s", "10s", 10.0 * NS),
        ("30s", "30s", 30.0 * NS),
        ("1m", "1m", 60.0 * NS),
        ("5m", "5m", 300.0 * NS),
        ("10m", "10m", 600.0 * NS),
        ("30m", "30m", 1800.0 * NS),
        ("1h", "1h", 3600.0 * NS),
        ("3h", "3h", 3.0 * 3600.0 * NS),
        ("12h", "12h", 12.0 * 3600.0 * NS),
        ("1d", "day", 86_400.0 * NS),
        ("7d", "week_sunday", 604_800.0 * NS),
        ("1M", "month", 2_629_746.0 * NS),
        ("3M", "quarter", 7_889_238.0 * NS),
        ("1y", "year", 31_556_952.0 * NS),
    ];
    let (label, unit) = STEPS
        .iter()
        .find(|(_, _, len)| (span_ns / len).floor() + 1.0 <= want as f64)
        .map(|(l, u, _)| (*l, *u))
        .unwrap_or(("1y", "year"));

    let fixed = unit.chars().all(|c| c.is_ascii_digit() || matches!(c, 's' | 'm' | 'h'));
    let mut request = json!({
        "date_histogram": {
            "field": field,
            (if fixed { "fixed_interval" } else { "calendar_interval" }): unit,
            // the buckets run unbroken from the first value to the last
            "min_doc_count": 0,
        },
    });
    if let Some(f) = spec.get("format") {
        request["date_histogram"]["format"] = f.clone();
    }
    if let Some(z) = spec.get("time_zone") {
        request["date_histogram"]["time_zone"] = z.clone();
    }
    if let Some(sa) = sub_aggs {
        request["aggs"] = sa;
    }
    let mut out = run_calendar_histogram(store, targets, main_query, &request)?;
    // the keys are written the way the request asked for them
    if let Some(format) = spec.get("format").and_then(|f| f.as_str())
        && let Some(buckets) = out.get_mut("buckets").and_then(|b| b.as_array_mut())
    {
        for b in buckets.iter_mut() {
            if let Some(ms) = b.get("key").and_then(|k| k.as_i64())
                && let Some(text) = crate::store::format_millis(ms, format)
            {
                b["key_as_string"] = json!(text);
            }
        }
    }
    out["interval"] = json!(label);
    Ok(out)
}
