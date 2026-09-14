//! How a bucket key is written once the answer is settled.

use crate::search::*;

// a composite walks the whole index in one pass, which it cannot do once
// per bucket of something else
pub(crate) fn composite_under_a_parent(node: &Value) -> bool {
    // an aggregation that produces one bucket does not multiply the work,
    // so a composite may sit inside one of those
    const SINGLE: &[&str] = &[
        "filter",
        "global",
        "nested",
        "reverse_nested",
        "sampler",
        "diversified_sampler",
        "missing",
        "children",
        "parent",
    ];
    let Some(o) = node.as_object() else { return false };
    o.values().any(|def| {
        let Some(subs) = def.get("aggs").or_else(|| def.get("aggregations")) else {
            return false;
        };
        let single = def
            .as_object()
            .map(|d| d.keys().any(|k| SINGLE.contains(&k.as_str())))
            .unwrap_or(false);
        if !single
            && subs
                .as_object()
                .map(|m| m.values().any(|d| d.get("composite").is_some()))
                .unwrap_or(false)
        {
            return true;
        }
        composite_under_a_parent(subs)
    })
}

// a date bucket is named to the millisecond, which is the resolution the
// key itself is counted in
pub(crate) fn millis_in_keys(node: &mut Value) {
    match node {
        Value::Object(o) => {
            if let Some(Value::String(text)) = o.get("key_as_string")
                && text.len() == 20
                && text.ends_with('Z')
                && !text.contains('.')
            {
                let with = format!("{}.000Z", &text[..text.len() - 1]);
                o.insert("key_as_string".into(), json!(with));
            }
            for (_, v) in o.iter_mut() {
                millis_in_keys(v);
            }
        }
        Value::Array(a) => {
            for v in a {
                millis_in_keys(v);
            }
        }
        _ => {}
    }
}

// a range aggregation answers for the ranges it was given; a gap between
// two of them was not asked about and is not a bucket

/// The metrics OpenSearch answers with a whole number rather than a fraction.
///
/// `value_count` counts values and `cardinality` counts distinct ones; both
/// are longs there, and a client that reads them into an integer cannot read
/// `3.0`.
pub(crate) fn whole_metric_values(result: &mut Value, req: &Value) {
    let Some(reqo) = req.as_object() else { return };
    for (name, def) in reqo {
        let Some(defo) = def.as_object() else { continue };
        let Some(node) = result.get_mut(name) else { continue };
        if (defo.contains_key("value_count") || defo.contains_key("cardinality"))
            && let Some(v) = node.get("value").and_then(|v| v.as_f64())
            && v.fract() == 0.0
        {
            node["value"] = json!(v as i64);
        }
        let Some(subs) = defo.get("aggs").or_else(|| defo.get("aggregations")) else { continue };
        match node.get_mut("buckets") {
            Some(Value::Array(list)) => {
                for b in list.iter_mut() {
                    whole_metric_values(b, subs);
                }
            }
            Some(Value::Object(named)) => {
                for (_, b) in named.iter_mut() {
                    whole_metric_values(b, subs);
                }
            }
            _ => whole_metric_values(node, subs),
        }
    }
}

pub(crate) fn keep_asked_ranges(request: &Value, answer: &mut Value) {
    let Some(reqs) = request.as_object() else { return };
    for (name, def) in reqs {
        #[allow(clippy::type_complexity)]
        let asked: Option<Vec<((Option<f64>, Option<f64>), Option<String>)>> =
            def.pointer("/range/ranges").and_then(|r| r.as_array()).map(|a| {
                a.iter()
                    .map(|r| {
                        (
                            (
                                r.get("from").and_then(|v| v.as_f64()),
                                r.get("to").and_then(|v| v.as_f64()),
                            ),
                            r.get("key").and_then(|v| v.as_str()).map(|s| s.to_string()),
                        )
                    })
                    .collect()
            });
        let Some(node) = answer.get_mut(name) else { continue };
        if let Some(asked) = asked
            && let Some(buckets) = node.get_mut("buckets").and_then(|b| b.as_array_mut())
        {
            let pair_of = |b: &Value| {
                (b.get("from").and_then(|v| v.as_f64()), b.get("to").and_then(|v| v.as_f64()))
            };
            buckets.retain(|b| asked.iter().any(|(p, _)| *p == pair_of(b)));
            // a range written with a key of its own is answered by that key
            // rather than by the bounds it stands for
            for b in buckets.iter_mut() {
                let pair = pair_of(b);
                if let Some((_, Some(key))) = asked.iter().find(|(p, k)| *p == pair && k.is_some())
                {
                    b["key"] = json!(key);
                }
            }
        }
        let subs = def.get("aggs").or_else(|| def.get("aggregations"));
        if let Some(subs) = subs {
            match node.get_mut("buckets") {
                Some(Value::Array(list)) => {
                    for b in list.iter_mut() {
                        keep_asked_ranges(subs, b);
                    }
                }
                Some(Value::Object(named)) => {
                    for (_, b) in named.iter_mut() {
                        keep_asked_ranges(subs, b);
                    }
                }
                _ => keep_asked_ranges(subs, node),
            }
        }
    }
}

/// The way a value is written beside the number it is: the `format` an
/// aggregation names, or a date field's own spelling where it names none.
enum ValueFormat {
    Date { pattern: String, nanos: bool },
    Decimal(String),
}

impl ValueFormat {
    fn write(&self, v: f64) -> Option<String> {
        match self {
            // the reference turns the double into a long the way Java casts,
            // which saturates rather than wraps
            ValueFormat::Date { pattern, nanos } => {
                let millis = if *nanos { (v / 1e6) as i64 } else { v as i64 };
                java_date(millis, pattern)
            }
            ValueFormat::Decimal(pattern) => decimal_format(pattern, v),
        }
    }
}

/// A date written the way the reference writes an instant, including the
/// ones past the year 9999 that a sum of dates reaches, which Java prints with
/// a sign in front of the year.
fn java_date(millis: i64, pattern: &str) -> Option<String> {
    let iso = matches!(
        pattern,
        "strict_date_optional_time" | "date_optional_time" | "iso8601" | "strict_date_time"
    );
    let days = millis.div_euclid(86_400_000);
    let rest = millis.rem_euclid(86_400_000);
    // days since the epoch to a civil date, after Howard Hinnant's algorithm
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let year_text = if year > 9999 {
        format!("+{year}")
    } else if year < 0 {
        format!("-{:04}", -year)
    } else {
        format!("{year:04}")
    };
    if !iso {
        // a year the calendar library cannot hold is put into the pattern
        // by hand; any other is the pattern's to write
        if (0..=9999).contains(&year) {
            return crate::store::format_millis(millis, pattern);
        }
        let mut out = String::new();
        let mut chars = pattern.chars().peekable();
        while let Some(c) = chars.next() {
            let mut run = 1;
            while chars.peek() == Some(&c) {
                chars.next();
                run += 1;
            }
            let field = match c {
                'y' | 'u' => year_text.clone(),
                'M' => format!("{month:0run$}"),
                'd' => format!("{day:0run$}"),
                'H' => format!("{:0run$}", rest / 3_600_000),
                'm' => format!("{:0run$}", rest / 60_000 % 60),
                's' => format!("{:0run$}", rest / 1000 % 60),
                'S' => format!("{:03}", rest % 1000),
                '\'' => String::new(),
                other => other.to_string().repeat(run),
            };
            out.push_str(&field);
        }
        return Some(out);
    }
    Some(format!(
        "{year_text}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    ))
}

/// The format an aggregation's values are written in, if any.
fn value_format(store: &Store, targets: &[String], spec: &Value) -> Option<ValueFormat> {
    let field = spec
        .get("field")
        .or_else(|| spec.pointer("/value/field"))
        .and_then(|f| f.as_str())
        .unwrap_or("");
    let ty = targets
        .iter()
        .filter_map(|n| store.get(n))
        .find_map(|st| st.read().mapping.type_of(field).map(|t| t.to_string()));
    let asked = spec.get("format").and_then(|f| f.as_str()).map(|s| s.to_string());
    match ty.as_deref() {
        Some(t @ ("date" | "date_nanos")) => Some(ValueFormat::Date {
            pattern: asked.unwrap_or_else(|| "strict_date_optional_time".to_string()),
            nanos: t == "date_nanos",
        }),
        _ => asked.map(ValueFormat::Decimal),
    }
}

/// Write each metric's value the way its format says, beside the number.
///
/// OpenSearch writes `value_as_string` -- and for the stats the `min_as_string`
/// and the rest -- whenever the value has a format other than the raw one: a
/// date field always does, and a number does once the request names a
/// `format`. Only the dates were written here, and always in ISO form, so a
/// `min` over a date asked for as `yyyy-MM-dd` came back with the full instant
/// and a `sum` asked for as `0.00` with no text at all.
pub(crate) fn name_date_metrics(
    store: &Store,
    targets: &[String],
    request: &Value,
    answer: &mut Value,
) {
    let Some(reqs) = request.as_object() else { return };
    for (name, def) in reqs {
        const SINGLE: &[&str] =
            &["avg", "min", "max", "sum", "median_absolute_deviation", "weighted_avg"];
        const STATS: &[&str] = &["stats", "extended_stats"];
        const PERCENTS: &[&str] = &["percentiles", "percentile_ranks"];
        let kind = def
            .as_object()
            .and_then(|o| {
                o.keys().map(|k| k.to_string()).find(|k| {
                    SINGLE.contains(&k.as_str())
                        || STATS.contains(&k.as_str())
                        || PERCENTS.contains(&k.as_str())
                        || matches!(k.as_str(), "terms" | "range")
                })
            })
            .unwrap_or_default();
        let format = def.get(&kind).and_then(|spec| value_format(store, targets, spec));
        if let (Some(format), Some(node)) = (format.as_ref(), answer.get_mut(name))
            && let Some(o) = node.as_object_mut()
        {
            let number =
                |o: &serde_json::Map<String, Value>, k: &str| o.get(k).and_then(|v| v.as_f64());
            if SINGLE.contains(&kind.as_str()) {
                if let Some(text) = number(o, "value").and_then(|v| format.write(v)) {
                    o.insert("value_as_string".into(), json!(text));
                }
            } else if STATS.contains(&kind.as_str()) {
                if o.get("count").and_then(|c| c.as_u64()).unwrap_or(0) > 0 {
                    let mut keys = vec!["min", "max", "avg", "sum"];
                    if kind == "extended_stats" {
                        keys.extend([
                            "sum_of_squares",
                            "variance",
                            "variance_population",
                            "variance_sampling",
                            "std_deviation",
                            "std_deviation_population",
                            "std_deviation_sampling",
                        ]);
                    }
                    for k in keys {
                        if let Some(text) = number(o, k).and_then(|v| format.write(v)) {
                            o.insert(format!("{k}_as_string"), json!(text));
                        }
                    }
                    if let Some(Value::Object(bounds)) = o.get("std_deviation_bounds") {
                        let written: serde_json::Map<String, Value> = bounds
                            .iter()
                            .filter_map(|(k, v)| {
                                Some((k.clone(), json!(format.write(v.as_f64()?)?)))
                            })
                            .collect();
                        o.insert("std_deviation_bounds_as_string".into(), Value::Object(written));
                    }
                }
            } else if PERCENTS.contains(&kind.as_str()) {
                match o.get_mut("values") {
                    Some(Value::Object(values)) => {
                        let mut out = serde_json::Map::new();
                        for (k, v) in values.iter() {
                            if k.ends_with("_as_string") {
                                continue;
                            }
                            out.insert(k.clone(), v.clone());
                            if let Some(text) = v.as_f64().and_then(|n| format.write(n)) {
                                out.insert(format!("{k}_as_string"), json!(text));
                            }
                        }
                        *values = out;
                    }
                    Some(Value::Array(values)) => {
                        for entry in values.iter_mut() {
                            if let Some(text) = entry
                                .get("value")
                                .and_then(|n| n.as_f64())
                                .and_then(|n| format.write(n))
                            {
                                entry["value_as_string"] = json!(text);
                            }
                        }
                    }
                    _ => {}
                }
            } else if let (ValueFormat::Decimal(_), Some(Value::Array(buckets))) =
                (format, o.get_mut("buckets"))
            {
                // a number bucketed under a format is named in it too: a
                // terms key, and the edges of a range
                for b in buckets.iter_mut().filter_map(|b| b.as_object_mut()) {
                    let edges: &[&str] = if kind == "terms" { &["key"] } else { &["from", "to"] };
                    for edge in edges {
                        let Some(text) = number(b, edge).and_then(|v| format.write(v)) else {
                            continue;
                        };
                        let named = if *edge == "key" {
                            "key_as_string".to_string()
                        } else {
                            format!("{edge}_as_string")
                        };
                        b.insert(named, json!(text));
                    }
                }
            }
        }
        let subs = def.get("aggs").or_else(|| def.get("aggregations"));
        let Some(subs) = subs else { continue };
        let Some(node) = answer.get_mut(name) else { continue };
        match node.get_mut("buckets") {
            Some(Value::Array(list)) => {
                for b in list.iter_mut() {
                    name_date_metrics(store, targets, subs, b);
                }
            }
            Some(Value::Object(named)) => {
                for (_, b) in named.iter_mut() {
                    name_date_metrics(store, targets, subs, b);
                }
            }
            _ => name_date_metrics(store, targets, subs, node),
        }
    }
}

/// Put the aggregations in the order the reference answers them in.
///
/// OpenSearch gathers the aggregations of every shard by name into a Java
/// `HashMap` before reducing them, at the top and inside every bucket, and
/// writes them in the order that map iterates: by the bucket each name's hash
/// lands in, and by the request between two that share one. The pipelines are
/// not shard aggregations; they are added after the rest, in the order they
/// were asked for. Here the aggregations are answered along several paths --
/// BoostCore's, the ones walked a bucket at a time, the pipelines -- and each
/// path's answers were laid in as they came, so the names came back in an
/// order neither the request nor the reference has. What is not an
/// aggregation -- a bucket's key, its count -- keeps its place in front.
pub(crate) fn order_as_requested(answer: &mut Value, request: &Value) {
    let (Some(reqs), Some(o)) = (request.as_object(), answer.as_object_mut()) else { return };
    let mut rest = serde_json::Map::new();
    let mut named: Vec<(String, Value)> = Vec::new();
    for (k, v) in std::mem::take(o) {
        match reqs.contains_key(&k) {
            true => named.push((k, v)),
            false => {
                rest.insert(k, v);
            }
        }
    }
    let pipeline = |d: &Value| {
        is_pipeline_agg(d)
            || d.as_object()
                .map(|o| o.keys().any(|k| BUCKET_PIPELINES.contains(&k.as_str())))
                .unwrap_or(false)
    };
    let shard_aggs: Vec<&String> =
        reqs.iter().filter(|(_, d)| !pipeline(d)).map(|(k, _)| k).collect();
    // a HashMap starts with sixteen buckets and doubles once it holds more
    // than three quarters of them
    let mut table = 16usize;
    while shard_aggs.len() * 4 > table * 3 {
        table *= 2;
    }
    let slot = |name: &str| -> usize {
        let h = name.encode_utf16().fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(c as u32));
        ((h ^ (h >> 16)) as usize) & (table - 1)
    };
    named.sort_by_key(|(k, _)| {
        let asked = reqs.keys().position(|r| r == k).unwrap_or(usize::MAX);
        match shard_aggs.contains(&k) {
            true => (0, slot(k), asked),
            false => (1, 0, asked),
        }
    });
    rest.extend(named);
    *o = rest;
    for (name, def) in reqs {
        let Some(subs) = def.get("aggs").or_else(|| def.get("aggregations")) else { continue };
        let Some(node) = o.get_mut(name) else { continue };
        match node.get_mut("buckets") {
            Some(Value::Array(list)) => list.iter_mut().for_each(|b| order_as_requested(b, subs)),
            Some(Value::Object(keyed)) => {
                keyed.values_mut().for_each(|b| order_as_requested(b, subs))
            }
            _ => order_as_requested(node, subs),
        }
    }
}

// pre-filtering lets a shard that cannot match be skipped entirely, but at
// least one always runs so there is a real (empty) result to return
// an aggregation that needs every shard (a `global`, or a bucket agg asking
// for empty buckets) defeats pre-filtering
pub(crate) fn needs_all_shards(node: &Value) -> bool {
    match node {
        Value::Object(o) => {
            o.contains_key("global")
                || o.get("min_doc_count").and_then(|v| v.as_i64()) == Some(0)
                || o.values().any(needs_all_shards)
        }
        Value::Array(a) => a.iter().any(needs_all_shards),
        _ => false,
    }
}

/// Does this field's terms live somewhere include/exclude cannot reach?
///
/// Both are matched against the term dictionary. An address is in there, but
/// as the fixed-width form rather than as it was written; a date is not in
/// there at all, since a date column is numeric. Either way the filter has to
/// come off the request and be applied to the answer instead.
pub(crate) fn term_filter_needs_translating(ty: Option<&str>) -> bool {
    matches!(ty, Some("ip" | "date" | "date_nanos"))
}

/// Render bucket keys through the `format` an aggregation asked for.
///
/// The pattern is Java's decimal format. Only the shape that appears in
/// practice is handled -- literal text around a run of `#` and `0`, where the
/// zeros after the point set how many decimals to show -- rather than the
/// whole grammar.
pub(crate) fn apply_bucket_formats(result: &mut Value, req: &Value) {
    let Some(reqo) = req.as_object() else { return };
    for (name, def) in reqo {
        let Some(defo) = def.as_object() else { continue };
        let Some(node) = result.get_mut(name) else { continue };
        let format = defo
            .values()
            .next()
            .and_then(|body| body.get("format"))
            .and_then(|f| f.as_str())
            .map(|s| s.to_string());
        if let (Some(fmt), Some(Value::Array(buckets))) = (&format, node.get_mut("buckets")) {
            for b in buckets.iter_mut() {
                let Some(o) = b.as_object_mut() else { continue };
                let Some(n) = o.get("key").and_then(|k| k.as_f64()) else { continue };
                if let Some(text) = decimal_format(fmt, n) {
                    o.insert("key_as_string".into(), Value::String(text));
                }
            }
        }
        let Some(sub) = defo.get("aggs").or_else(|| defo.get("aggregations")) else { continue };
        match node.get_mut("buckets") {
            Some(Value::Array(buckets)) => {
                for b in buckets.iter_mut() {
                    apply_bucket_formats(b, sub);
                }
            }
            _ => apply_bucket_formats(node, sub),
        }
    }
}

/// `Value is ##0.0` applied to 50 gives `Value is 50.0`.
///
/// Read the way Java's `DecimalFormat` reads a pattern: the zeros before the
/// point are the fewest integer digits, the zeros after it the fewest
/// fraction digits and the `#`s the most, a comma sets the grouping, and the
/// value is rounded half to even. Only the decimals were read, so `000` wrote
/// 20 as `20` where the reference writes `020`, and `#,##0.0` wrote no comma.
pub(crate) fn decimal_format(pattern: &str, value: f64) -> Option<String> {
    // a pattern for negative numbers after `;` is not read; the reference
    // uses the positive one with a minus sign when it is left out
    let pattern = pattern.split(';').next().unwrap_or(pattern);
    let start = pattern.find(['#', '0'])?;
    let end = pattern.rfind(['#', '0'])? + 1;
    let (prefix, numeric, suffix) = (&pattern[..start], &pattern[start..end], &pattern[end..]);
    if !value.is_finite() {
        return Some(if value.is_nan() {
            "NaN".to_string()
        } else {
            format!("{}\u{221e}", if value < 0.0 { "-" } else { "" })
        });
    }
    let value = if prefix.contains('%') || suffix.contains('%') { value * 100.0 } else { value };
    let (int_part, frac_part) = numeric.split_once('.').unwrap_or((numeric, ""));
    let min_int = int_part.chars().filter(|c| *c == '0').count();
    let grouping = int_part.rfind(',').map(|at| int_part[at + 1..].len());
    let min_frac = frac_part.chars().filter(|c| *c == '0').count();
    let max_frac = frac_part.chars().filter(|c| matches!(c, '0' | '#')).count();
    // Rust writes the exact binary value rounded half to even, which is what
    // `DecimalFormat` does with a double since Java 8
    let fixed = format!("{:.max_frac$}", value.abs());
    let (whole, frac) = fixed.split_once('.').unwrap_or((&fixed, ""));
    let frac = frac.trim_end_matches('0');
    let frac = if frac.len() < min_frac { format!("{frac:0<min_frac$}") } else { frac.to_string() };
    let whole = whole.trim_start_matches('0');
    let whole =
        if whole.len() < min_int { format!("{whole:0>min_int$}") } else { whole.to_string() };
    let whole = match grouping.filter(|g| *g > 0) {
        Some(size) => {
            let digits: Vec<char> = whole.chars().collect();
            let mut out = String::new();
            for (i, c) in digits.iter().enumerate() {
                if i > 0 && (digits.len() - i).is_multiple_of(size) {
                    out.push(',');
                }
                out.push(*c);
            }
            out
        }
        None => whole,
    };
    let mut body = match frac.is_empty() {
        true => whole,
        false => format!("{whole}.{frac}"),
    };
    // nothing left to write is written as a zero
    if body.is_empty() {
        body = "0".to_string();
    }
    let negative = value < 0.0 && body.chars().any(|c| c.is_ascii_digit() && c != '0');
    let sign = if negative { "-" } else { "" };
    Some(format!("{sign}{prefix}{body}{suffix}"))
}

/// Write each `terms` bucket key in the spelling its field is read in.
///
/// An address is stored in the fixed-width form that sorts correctly and a
/// date as text; neither is what the field was given, so the request is walked
/// alongside the answer to find which field each set of buckets came from.
/// Write a terms aggregation's numeric keys as fractions when any index in
/// the search holds the field that way.
///
/// Two indices can disagree: one stores whole numbers, the other fractions.
/// The buckets merge on value regardless, but a key written back as `10` where
/// another document contributed `10.0` reports a field that has two types.
/// Ties in a count are settled by key, which is the order that produces.
pub(crate) fn widen_number_keys(
    result: &mut Value,
    req: &Value,
    floating: &std::collections::HashSet<String>,
) {
    let Some(reqo) = req.as_object() else { return };
    for (name, def) in reqo {
        let Some(defo) = def.as_object() else { continue };
        let Some(node) = result.get_mut(name) else { continue };
        // the terms under another bucket are ordered the same way; the
        // sub-aggregations were looked for on the aggregation itself rather
        // than in its buckets, so only the top level was ever put in order
        if let Some(sub) = defo.get("aggs").or_else(|| defo.get("aggregations")) {
            match node.get_mut("buckets") {
                Some(Value::Array(list)) => {
                    list.iter_mut().for_each(|b| widen_number_keys(b, sub, floating))
                }
                Some(Value::Object(keyed)) => {
                    keyed.values_mut().for_each(|b| widen_number_keys(b, sub, floating))
                }
                _ => widen_number_keys(node, sub, floating),
            }
        }
        let Some(terms) = defo.get("terms") else { continue };
        let field = terms.get("field").and_then(|f| f.as_str()).unwrap_or("");
        let Some(Value::Array(buckets)) = node.get_mut("buckets") else { continue };
        if floating.contains(field) {
            for b in buckets.iter_mut() {
                let Some(o) = b.as_object_mut() else { continue };
                let widened = o.get("key").and_then(|k| k.as_i64()).map(|i| i as f64);
                if let Some(f) = widened {
                    o.insert("key".into(), json!(f));
                }
            }
        }
        // an explicit order is the caller's, and is left alone
        if terms.get("order").is_some() {
            continue;
        }
        // buckets go by count, and a tie between them by key -- ascending,
        // which for a string is its own order and for a number is its value
        buckets.sort_by(|a, b| {
            let count = |v: &Value| v.get("doc_count").and_then(|c| c.as_u64()).unwrap_or(0);
            let key = |v: &Value| match v.get("key") {
                Some(Value::Number(n)) => (None, n.as_f64().unwrap_or(f64::MAX)),
                Some(Value::String(s)) => (Some(s.clone()), 0.0),
                _ => (None, f64::MAX),
            };
            let (ka, na) = key(a);
            let (kb, nb) = key(b);
            count(b).cmp(&count(a)).then_with(|| match (&ka, &kb) {
                (Some(x), Some(y)) => x.cmp(y),
                _ => na.partial_cmp(&nb).unwrap_or(Ordering::Equal),
            })
        });
    }
}

/// Cut each terms answer back to the size it was asked for.
///
/// BoostCore was asked for more buckets than wanted, so that a tie at the
/// last one is settled the reference's way: by count, and then by the smaller
/// key. What is cut off is counted among the other documents.
pub(crate) fn cut_terms(result: &mut Value, req: &Value) {
    let Some(reqo) = req.as_object() else { return };
    for (name, def) in reqo {
        let Some(node) = result.get_mut(name) else { continue };
        if let Some(sub) = def.get("aggs").or_else(|| def.get("aggregations")) {
            match node.get_mut("buckets") {
                Some(Value::Array(list)) => list.iter_mut().for_each(|b| cut_terms(b, sub)),
                Some(Value::Object(keyed)) => keyed.values_mut().for_each(|b| cut_terms(b, sub)),
                _ => cut_terms(node, sub),
            }
        }
        let Some(terms) = def.get("terms") else { continue };
        let by_count = match terms.get("order") {
            None => true,
            Some(o) => o.get("_count").and_then(|v| v.as_str()) == Some("desc"),
        };
        if !by_count || terms.get("include").and_then(|i| i.get("partition")).is_some() {
            continue;
        }
        let size = terms.get("size").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
        let Some(Value::Array(buckets)) = node.get_mut("buckets") else { continue };
        if buckets.len() <= size {
            continue;
        }
        buckets.sort_by(|a, b| {
            let count = |v: &Value| v.get("doc_count").and_then(|c| c.as_u64()).unwrap_or(0);
            let key = |v: &Value| match v.get("key") {
                Some(Value::Number(n)) => (None, n.as_f64().unwrap_or(f64::MAX)),
                Some(Value::String(s)) => (Some(s.clone()), 0.0),
                _ => (None, f64::MAX),
            };
            let (ka, na) = key(a);
            let (kb, nb) = key(b);
            count(b).cmp(&count(a)).then_with(|| match (&ka, &kb) {
                (Some(x), Some(y)) => x.cmp(y),
                _ => na.partial_cmp(&nb).unwrap_or(Ordering::Equal),
            })
        });
        let cut: u64 = buckets
            .drain(size..)
            .map(|b| b.get("doc_count").and_then(|c| c.as_u64()).unwrap_or(0))
            .sum();
        let other = node.get("sum_other_doc_count").and_then(|v| v.as_u64()).unwrap_or(0);
        node["sum_other_doc_count"] = json!(other + cut);
    }
}

pub(crate) fn format_terms_keys(
    result: &mut Value,
    req: &Value,
    types: &std::collections::HashMap<String, String>,
) {
    let Some(reqo) = req.as_object() else { return };
    for (name, def) in reqo {
        let Some(defo) = def.as_object() else { continue };
        let Some(node) = result.get_mut(name) else { continue };

        // A `terms` result says how far its counts may be off, and with
        // `show_term_doc_count_error` so does each bucket. Some of the paths
        // that answer a terms aggregation -- ordered by key, by a
        // sub-aggregation -- left the bound out, and a client reading it
        // found nothing there.
        if let Some(terms) = defo.get("terms")
            && let Some(o) = node.as_object_mut()
            && o.contains_key("buckets")
        {
            o.entry("doc_count_error_upper_bound").or_insert(json!(0));
            if terms.get("show_term_doc_count_error").and_then(|v| v.as_bool()) == Some(true)
                && let Some(Value::Array(buckets)) = o.get_mut("buckets")
            {
                for b in buckets.iter_mut().filter_map(|b| b.as_object_mut()) {
                    b.entry("doc_count_error_upper_bound").or_insert(json!(0));
                }
            }
        }

        if defo.contains_key("terms") {
            let field = defo
                .get("terms")
                .and_then(|t| t.get("field"))
                .and_then(|f| f.as_str())
                .unwrap_or("");
            let base = match field.strip_suffix(".keyword") {
                Some(parent)
                    if !matches!(
                        types.get(parent).map(|s| s.as_str()),
                        Some("object" | "nested")
                    ) =>
                {
                    parent
                }
                _ => field,
            };
            let ty = types.get(base).cloned();
            let listed = |key: &str| -> Option<Vec<String>> {
                let v = defo.get("terms")?.get(key)?;
                Some(match v {
                    Value::Array(a) => a.iter().filter_map(term_filter_text).collect(),
                    other => term_filter_text(other).into_iter().collect(),
                })
            };
            let translating = term_filter_needs_translating(ty.as_deref());
            let include = translating.then(|| listed("include")).flatten();
            let exclude = translating.then(|| listed("exclude")).flatten();

            if let Some(Value::Array(buckets)) = node.get_mut("buckets") {
                for b in buckets.iter_mut() {
                    let Some(o) = b.as_object_mut() else { continue };
                    let Some(raw) = o.get("key").cloned() else { continue };
                    let (key, as_string) = terms_key_view(raw, ty.as_deref());
                    o.insert("key".into(), key);
                    match as_string {
                        Some(text) => {
                            o.insert("key_as_string".into(), Value::String(text));
                        }
                        None => {
                            o.remove("key_as_string");
                        }
                    }
                }
                // the filters that could not be pushed down are applied here
                if include.is_some() || exclude.is_some() {
                    buckets.retain(|b| {
                        let shown = (
                            b.get("key").cloned().unwrap_or(Value::Null),
                            b.get("key_as_string").and_then(|s| s.as_str()).map(|s| s.to_string()),
                        );
                        let hit = |list: &Vec<String>| {
                            list.iter().any(|want| term_filter_matches(want, &shown, ty.as_deref()))
                        };
                        include.as_ref().map(hit).unwrap_or(true)
                            && !exclude.as_ref().map(hit).unwrap_or(false)
                    });
                }
            }
        }

        let Some(sub) = defo.get("aggs").or_else(|| defo.get("aggregations")) else { continue };
        match node.get_mut("buckets") {
            Some(Value::Array(buckets)) => {
                for b in buckets.iter_mut() {
                    format_terms_keys(b, sub, types);
                }
            }
            Some(Value::Object(keyed)) => {
                for (_, b) in keyed.iter_mut() {
                    format_terms_keys(b, sub, types);
                }
            }
            _ => format_terms_keys(node, sub, types),
        }
    }
}

/// A numeric range bucket names its bounds as doubles.
///
/// BoostCore writes `*-50` where the suite expects `*-50.0`; the bounds are
/// already on the bucket, so the key is rebuilt from them rather than parsed.
pub(crate) fn normalize_range_keys(node: &mut Value) {
    match node {
        Value::Object(o) => {
            if let Some(Value::Array(buckets)) = o.get_mut("buckets") {
                for b in buckets.iter_mut() {
                    let numeric = b.get("from").map(|v| v.is_number()).unwrap_or(false)
                        || b.get("to").map(|v| v.is_number()).unwrap_or(false);
                    let has_key = b.get("key").map(|k| k.is_string()).unwrap_or(false);
                    if !numeric || !has_key {
                        continue;
                    }
                    let show = |v: Option<&Value>| match v.and_then(|x| x.as_f64()) {
                        Some(n) if n.is_finite() => {
                            if n.fract() == 0.0 && n.abs() < 1e15 {
                                format!("{n:.1}")
                            } else {
                                format!("{n}")
                            }
                        }
                        _ => "*".to_string(),
                    };
                    let key = format!("{}-{}", show(b.get("from")), show(b.get("to")));
                    b["key"] = json!(key);
                }
            }
            // Keyed, the buckets are an object named by the same key, with no
            // key inside each, in the order of the ranges. Only the list form
            // was put right, so a keyed range was named `*-3` where the
            // reference names it `*-3.0`, carried a `key` it does not, and
            // came back last range first.
            if let Some(Value::Object(keyed)) = o.get("buckets") {
                let numeric = keyed.values().any(|b| {
                    b.get("from").map(|v| v.is_number()).unwrap_or(false)
                        || b.get("to").map(|v| v.is_number()).unwrap_or(false)
                });
                if numeric {
                    let show = |v: Option<&Value>| match v.and_then(|x| x.as_f64()) {
                        Some(n) if n.is_finite() => {
                            if n.fract() == 0.0 && n.abs() < 1e15 {
                                format!("{n:.1}")
                            } else {
                                format!("{n}")
                            }
                        }
                        _ => "*".to_string(),
                    };
                    let edge = |b: &Value, k: &str, open: f64| {
                        b.get(k).and_then(|v| v.as_f64()).unwrap_or(open)
                    };
                    let mut entries: Vec<(String, Value)> =
                        keyed.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    entries.sort_by(|(_, a), (_, b)| {
                        edge(a, "from", f64::NEG_INFINITY)
                            .total_cmp(&edge(b, "from", f64::NEG_INFINITY))
                            .then_with(|| {
                                edge(a, "to", f64::INFINITY).total_cmp(&edge(
                                    b,
                                    "to",
                                    f64::INFINITY,
                                ))
                            })
                    });
                    let mut rebuilt = serde_json::Map::new();
                    for (name, mut b) in entries {
                        // a key the caller gave a range is kept as they wrote it
                        let generated = b.get("key").and_then(|k| k.as_str())
                            == Some(name.as_str())
                            && name.contains('-');
                        let named = if generated {
                            format!("{}-{}", show(b.get("from")), show(b.get("to")))
                        } else {
                            name
                        };
                        if let Some(bo) = b.as_object_mut() {
                            bo.remove("key");
                        }
                        rebuilt.insert(named, b);
                    }
                    o.insert("buckets".into(), Value::Object(rebuilt));
                }
            }
            for (_, v) in o.iter_mut() {
                normalize_range_keys(v);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(normalize_range_keys),
        _ => {}
    }
}

/// `ip_range`: one bucket per address range.
///
/// Each range is a filter on the field, so the ordinary query path answers it;
/// `from` is included and `to` is not, and either may be left open.
/// Is this a field no index in the search knows anything about -- neither
/// mapped nor ever seen in a document?
pub(crate) fn unmapped_field(store: &Store, targets: &[String], field: &str) -> bool {
    !targets.iter().filter_map(|n| store.get(n)).any(|st| {
        let g = st.read();
        g.mapping.type_of(field).is_some() || g.observed_kinds.contains_key(field)
    })
}

/// Is this field one of the range types, which store two endpoints per
/// document rather than one value?
pub(crate) fn range_field(store: &Store, targets: &[String], field: &str) -> bool {
    targets
        .iter()
        .filter_map(|n| store.get(n))
        .any(|st| st.read().mapping.type_of(field).map(|t| t.ends_with("_range")).unwrap_or(false))
}

/// One entry of an include/exclude list, as text.
pub(crate) fn term_filter_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Does one include/exclude entry name this bucket?
///
/// The caller writes a date or an address the way it was sent; the bucket
/// carries the way it is read back. Both are put in one spelling before they
/// are compared.
pub(crate) fn term_filter_matches(
    want: &str,
    shown: &(Value, Option<String>),
    ty: Option<&str>,
) -> bool {
    let (key, as_string) = shown;
    match ty {
        Some("date") | Some("date_nanos") => {
            let a = crate::store::canonical_date(&Value::String(want.to_string()));
            let b = as_string.clone().and_then(|s| crate::store::canonical_date(&Value::String(s)));
            a.is_some() && a == b
        }
        Some("ip") => {
            let a = crate::store::canonical_ip(want);
            let b = key.as_str().and_then(crate::store::canonical_ip);
            a.is_some() && a == b
        }
        _ => {
            let text = match key {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            text == want
        }
    }
}

/// How a term key and its readable form are written for a field of this type.
pub(crate) fn terms_key_view(raw: Value, ty: Option<&str>) -> (Value, Option<String>) {
    match ty {
        Some("ip") => {
            let shown = raw.as_str().and_then(crate::store::ip_from_canonical);
            (shown.map(Value::String).unwrap_or(raw), None)
        }
        Some("boolean") => {
            let n = raw.as_u64().unwrap_or(0);
            (json!(n), Some(if n != 0 { "true".into() } else { "false".into() }))
        }
        Some(ty @ ("date" | "date_nanos")) => {
            // a date key is the number the index holds -- milliseconds, or
            // nanoseconds for a date_nanos -- and is shown as a date besides
            let Some(n) = raw.as_f64() else { return (raw, None) };
            let millis = if ty == "date_nanos" { n / 1e6 } else { n } as i64;
            match crate::store::format_millis(millis, "strict_date_optional_time") {
                Some(text) => (json!(n as i64), Some(text)),
                None => (raw, None),
            }
        }
        _ => (raw, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decimal_pattern_is_read_the_way_java_reads_it() {
        // each read back from OpenSearch 3.8
        assert_eq!(decimal_format("000", 20.0).as_deref(), Some("020"));
        assert_eq!(decimal_format("00.0", 1.0).as_deref(), Some("01.0"));
        assert_eq!(decimal_format("#,##0.0", 343.1786085219707).as_deref(), Some("343.2"));
        assert_eq!(decimal_format("#,##0.0", 1030908.54).as_deref(), Some("1,030,908.5"));
        assert_eq!(decimal_format("0.000", 0.31).as_deref(), Some("0.310"));
        assert_eq!(decimal_format("0.000", -0.032998899453124314).as_deref(), Some("-0.033"));
        assert_eq!(decimal_format("0.00", 1030908.54).as_deref(), Some("1030908.54"));
        // half to even, on the value the double really holds
        assert_eq!(decimal_format("0.00", 0.125).as_deref(), Some("0.12"));
        assert_eq!(decimal_format("0.00", 1.005).as_deref(), Some("1.00"));
    }

    #[test]
    fn an_instant_past_the_year_9999_carries_a_sign() {
        assert_eq!(
            java_date(5268404129520000, "strict_date_optional_time").as_deref(),
            Some("+168919-01-30T15:32:00.000Z")
        );
        assert_eq!(java_date(5268404129520000, "yyyy-MM-dd").as_deref(), Some("+168919-01-30"));
        assert_eq!(
            java_date(1735726920000, "strict_date_optional_time").as_deref(),
            Some("2025-01-01T10:22:00.000Z")
        );
    }

    #[test]
    fn aggregations_come_back_in_the_order_a_java_hash_map_keeps() {
        let request = json!({
            "z": {"sum": {"field": "n"}},
            "b": {"max": {"field": "n"}},
            "p": {"max_bucket": {"buckets_path": "t>s"}},
            "m": {"min": {"field": "n"}},
            "a": {"avg": {"field": "n"}},
        });
        let mut answer = json!({"p": {}, "m": {}, "z": {}, "a": {}, "b": {}});
        order_as_requested(&mut answer, &request);
        let names: Vec<&String> =
            answer.as_object().map(|o| o.keys().collect()).unwrap_or_default();
        assert_eq!(names, ["a", "b", "z", "m", "p"]);
    }
}
