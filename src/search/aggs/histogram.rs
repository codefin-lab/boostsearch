//! Histograms over dates, where a calendar unit or a zone is involved.

use super::*;
use crate::search::*;

/// Write a date histogram's keys the way a date histogram writes them: the key
/// is a whole number of milliseconds, and it is named beside it.
pub(crate) fn date_histogram_keys(
    result: &mut Value,
    req: &Value,
    types: &std::collections::HashMap<String, String>,
) {
    let Some(reqo) = req.as_object() else { return };
    for (name, def) in reqo {
        let Some(defo) = def.as_object() else { continue };
        let Some(node) = result.get_mut(name) else { continue };
        if let (Some(spec), Some(sub)) =
            (defo.get("date_histogram"), defo.get("aggs").or_else(|| defo.get("aggregations")))
        {
            let _ = spec;
            match node.get_mut("buckets") {
                Some(Value::Array(buckets)) => {
                    for b in buckets.iter_mut() {
                        date_histogram_keys(b, sub, types);
                    }
                }
                Some(Value::Object(keyed)) => {
                    for (_, b) in keyed.iter_mut() {
                        date_histogram_keys(b, sub, types);
                    }
                }
                _ => date_histogram_keys(node, sub, types),
            }
        } else if let Some(sub) = defo.get("aggs").or_else(|| defo.get("aggregations")) {
            match node.get_mut("buckets") {
                Some(Value::Array(buckets)) => {
                    for b in buckets.iter_mut() {
                        date_histogram_keys(b, sub, types);
                    }
                }
                Some(Value::Object(keyed)) => {
                    for (_, b) in keyed.iter_mut() {
                        date_histogram_keys(b, sub, types);
                    }
                }
                _ => date_histogram_keys(node, sub, types),
            }
        }
        let Some(spec) = defo.get("date_histogram") else { continue };
        if walked_here(spec) {
            continue;
        }
        let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("");
        if types.get(field).map(|t| t.as_str()) != Some("date") {
            continue;
        }
        let format = spec
            .get("format")
            .and_then(|f| f.as_str())
            .unwrap_or("strict_date_optional_time")
            .to_string();
        let name_one = |b: &mut Value| {
            let Some(o) = b.as_object_mut() else { return };
            let Some(ms) = o.get("key").and_then(|k| k.as_f64()) else { return };
            o.insert("key".into(), json!(ms as i64));
            if let Some(text) = crate::store::format_millis(ms as i64, &format) {
                o.insert("key_as_string".into(), json!(text));
            }
        };
        match node.get_mut("buckets") {
            Some(Value::Array(buckets)) => buckets.iter_mut().for_each(name_one),
            Some(Value::Object(keyed)) => keyed.iter_mut().for_each(|(_, b)| name_one(b)),
            _ => {}
        }
    }
}

/// A date histogram stepped by calendar units.
///
/// A month is not a fixed number of milliseconds, so BoostCore's histogram --
/// which steps by a constant -- cannot express one. Each bucket is instead a
/// range filter run through the ordinary query path, which also means
/// sub-aggregations come for free. The cost is one search per bucket, which
/// suits the handful of buckets a calendar histogram usually spans.
pub(crate) fn run_calendar_histogram(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    use boostcore::time::{Duration, OffsetDateTime};

    let spec = def.get("date_histogram").cloned().unwrap_or(json!({}));
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    // a histogram steps by a calendar unit or by a fixed length; the fixed
    // one only comes through here when a zone means BoostCore cannot do it
    let fixed = spec
        .get("fixed_interval")
        .and_then(|v| v.as_str())
        .and_then(parse_offset)
        .filter(|d| d.whole_nanoseconds() > 0);
    let interval =
        spec.get("calendar_interval").and_then(|v| v.as_str()).unwrap_or("day").to_string();
    let unit = match fixed {
        Some(_) => CalendarUnit::Second,
        None => match CalendarUnit::parse(&interval) {
            Some(u) => u,
            None => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!(
                        "The supplied interval [{interval}] could not be parsed as a calendar \
                         interval."
                    ),
                ));
            }
        },
    };
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let min_doc_count = spec.get("min_doc_count").and_then(|v| v.as_u64()).unwrap_or(0);

    // the span to cover comes from the extremes the query actually matches
    // a range-typed field has no single value per document, so it has no
    // extremes to read; its span has to come from the bounds the request gives
    let ranged = targets.iter().filter_map(|n| store.get(n)).any(|st| {
        st.read().mapping.type_of(&field).map(|t| t.ends_with("_range")).unwrap_or(false)
    });
    let bounds = spec.get("hard_bounds").or_else(|| spec.get("extended_bounds"));
    // A date is a number in the index: milliseconds, or nanoseconds for a
    // date_nanos. A date_range keeps its endpoints as text, which BoostCore
    // reads back as a date column counting nanoseconds.
    let per_ns: f64 = targets
        .iter()
        .filter_map(|n| store.get(n))
        .find_map(|st| match st.read().mapping.type_of(&field) {
            Some("date_nanos") => Some(1.0),
            // a date_range holds its endpoints as dates do, in milliseconds
            Some(t) if t.starts_with("date") => Some(1_000_000.0),
            _ => None,
        })
        .unwrap_or(1.0);
    let (mut lo_ns, mut hi_ns) = (0.0f64, 0.0f64);
    if !ranged {
        let base = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
        let probe = json!({
            "__min": {"min": {"field": field}},
            "__max": {"max": {"field": field}},
        });
        // one index may hold the field as a date and another as a date_nanos,
        // so each is asked in its own unit before the two spans are joined
        let mut span: Option<(f64, f64)> = None;
        for target in targets {
            let one = std::slice::from_ref(target);
            let per = store
                .get(target)
                .map(|st| match st.read().mapping.type_of(&field) {
                    Some("date_nanos") => 1.0,
                    Some(t) if t.starts_with("date") => 1_000_000.0,
                    _ => 1.0,
                })
                .unwrap_or(per_ns);
            let (_, extremes) = filtered_count(store, one, &base, &Some(probe.clone()))?;
            let read =
                |k: &str| -> Option<f64> { extremes.as_ref()?.get(k)?.get("value")?.as_f64() };
            if let (Some(a), Some(b)) = (read("__min"), read("__max")) {
                let (a, b) = (a * per, b * per);
                span = Some(match span {
                    Some((lo, hi)) => (lo.min(a), hi.max(b)),
                    None => (a, b),
                });
            }
        }
        let Some((a, b)) = span else {
            return Ok(json!({"buckets": []}));
        };
        (lo_ns, hi_ns) = (a, b);
    } else if bounds.is_none() {
        // a range field has no single value, but the endpoints it stores do:
        // the span runs from the earliest start to the latest end
        let base = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
        let probe = json!({
            "__min": {"min": {"field": format!("{field}.gte")}},
            "__max": {"max": {"field": format!("{field}.lte")}},
        });
        let (_, extremes) = filtered_count(store, targets, &base, &Some(probe))?;
        let read = |k: &str| -> Option<f64> { extremes.as_ref()?.get(k)?.get("value")?.as_f64() };
        match (read("__min"), read("__max")) {
            (Some(a), Some(b)) => (lo_ns, hi_ns) = (a * per_ns, b * per_ns),
            _ => return Ok(json!({"buckets": []})),
        }
    }
    // bounds are written the way a document would be, so they arrive as a date
    // and have to meet the nanoseconds the calendar is walked in
    let bound_ns = |key: &str| -> Option<f64> {
        let v = bounds?.get(key)?;
        crate::store::canonical_date(v)
            .and_then(|d| crate::store::parse_date_lenient(&d))
            .map(|d| d.unix_timestamp_nanos() as f64)
    };
    let lo_ns = bound_ns("min").unwrap_or(lo_ns);
    let hi_ns = bound_ns("max").unwrap_or(hi_ns);

    let to_dt = |ns: f64| -> Option<OffsetDateTime> {
        OffsetDateTime::from_unix_timestamp_nanos(ns as i128).ok()
    };
    let (Some(lo), Some(hi)) = (to_dt(lo_ns), to_dt(hi_ns)) else {
        return Ok(json!({"buckets": []}));
    };

    // `offset` shifts the whole grid of boundaries. The buckets keep their
    // calendar width; they just no longer start on the calendar unit.
    let offset = spec
        .get("offset")
        .and_then(|v| v.as_str())
        .and_then(parse_offset)
        .unwrap_or(Duration::seconds(0));
    // a zone is not an offset but a history of them, so the offset in force
    // is the one at the instant being placed
    let zone = spec.get("time_zone").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let zone_at = |dt: OffsetDateTime| -> Duration {
        Duration::seconds(crate::tz::offset_at(&zone, dt.unix_timestamp()).unwrap_or(0) as i64)
    };
    let floor_local = |local: OffsetDateTime| -> OffsetDateTime {
        match fixed {
            // a fixed step divides the line evenly from the epoch
            Some(step) => {
                let step_ns = step.whole_nanoseconds();
                let ns = local.unix_timestamp_nanos();
                let floored = ns.div_euclid(step_ns) * step_ns;
                OffsetDateTime::from_unix_timestamp_nanos(floored).unwrap_or(local)
            }
            None => unit.floor(local - offset) + offset,
        }
    };
    let shift = |dt: OffsetDateTime| {
        let o = zone_at(dt);
        floor_local(dt + o) - o
    };

    let mut buckets = Vec::new();
    let mut cursor = shift(lo);
    let last = shift(hi);
    // a runaway interval would otherwise spin: no calendar histogram the suite
    // or a sane request produces comes near this
    let mut guard = 0;
    while cursor <= last && guard < 100_000 {
        guard += 1;
        let o = zone_at(cursor);
        let next = match fixed {
            Some(step) => cursor + step,
            None => (unit.advance((cursor + o) - offset) + offset) - o,
        };
        let mut spec = json!({
            "gte": iso_millis(cursor),
            "lt": iso_millis(next),
            "format": "strict_date_optional_time",
        });
        if ranged {
            // a stored interval belongs to every bucket it touches
            spec["relation"] = json!("intersects");
        }
        let range = json!({"range": {field.clone(): spec}});
        let combined = combine(main_query, Some(range));
        let (count, sub) = count_with_sub_aggs(store, targets, &combined, &sub_aggs, false)?;
        if count >= min_doc_count {
            let mut b = json!({
                "key": cursor.unix_timestamp_nanos() as i64 / 1_000_000,
                "key_as_string": iso_millis_at(cursor, &zone, o),
                "doc_count": count,
            });
            if let Some(Value::Object(o)) = sub {
                for (k, v) in o {
                    b[k] = v;
                }
            }
            buckets.push(b);
        }
        if next == cursor {
            break;
        }
        cursor = next;
    }
    // buckets are built in calendar order; `order` may want another. `_time`
    // is the old spelling of `_key`, and both name the bucket's own date.
    if let Some((key, desc)) = spec
        .get("order")
        .and_then(|o| o.as_object())
        .and_then(|o| o.iter().next())
        .map(|(k, v)| (k.clone(), v.as_str() == Some("desc")))
    {
        let by = |f: fn(&Value) -> i64| {
            move |a: &Value, b: &Value| if desc { f(b).cmp(&f(a)) } else { f(a).cmp(&f(b)) }
        };
        match key.as_str() {
            "_key" | "_time" => {
                buckets.sort_by(by(|x| x.get("key").and_then(|v| v.as_i64()).unwrap_or(0)))
            }
            "_count" => {
                buckets.sort_by(by(|x| x.get("doc_count").and_then(|v| v.as_i64()).unwrap_or(0)))
            }
            _ => {}
        }
    }
    let _ = Duration::seconds(0);
    Ok(json!({"buckets": buckets}))
}

/// `offset` as written on a date histogram: a signed count of fixed time
/// units. Calendar units are not allowed here -- only lengths that are the
/// same wherever on the calendar they land.
pub(crate) fn parse_offset(s: &str) -> Option<boostcore::time::Duration> {
    let s = s.trim();
    let (sign, rest) = match s.strip_prefix('-') {
        Some(r) => (-1, r),
        None => (1, s.strip_prefix('+').unwrap_or(s)),
    };
    let split = rest.find(|c: char| !c.is_ascii_digit())?;
    let (n, unit) = rest.split_at(split);
    let n: i64 = n.parse().ok()?;
    let n = n * sign;
    Some(match unit {
        "ms" => boostcore::time::Duration::milliseconds(n),
        "s" => boostcore::time::Duration::seconds(n),
        "m" => boostcore::time::Duration::minutes(n),
        "h" | "H" => boostcore::time::Duration::hours(n),
        "d" => boostcore::time::Duration::days(n),
        _ => return None,
    })
}

/// A date written in the zone it is being reported in, which is what puts the
/// offset on the end of it in place of the `Z`.
pub(crate) fn iso_millis_at(
    dt: boostcore::time::OffsetDateTime,
    zone: &str,
    offset: boostcore::time::Duration,
) -> String {
    if zone.is_empty() || offset.is_zero() {
        return iso_millis(dt);
    }
    let local = dt + offset;
    let total = offset.whole_minutes();
    let sign = if total < 0 { '-' } else { '+' };
    let total = total.abs();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}{}{:02}:{:02}",
        local.year(),
        local.month() as u8,
        local.day(),
        local.hour(),
        local.minute(),
        local.second(),
        local.millisecond(),
        sign,
        total / 60,
        total % 60,
    )
}

pub(crate) fn iso_millis(dt: boostcore::time::OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.millisecond(),
    )
}
