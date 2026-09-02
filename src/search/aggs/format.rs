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
pub(crate) fn keep_asked_ranges(request: &Value, answer: &mut Value) {
    let Some(reqs) = request.as_object() else { return };
    for (name, def) in reqs {
        let asked: Option<Vec<(Option<f64>, Option<f64>)>> =
            def.pointer("/range/ranges").and_then(|r| r.as_array()).map(|a| {
                a.iter()
                    .map(|r| {
                        (
                            r.get("from").and_then(|v| v.as_f64()),
                            r.get("to").and_then(|v| v.as_f64()),
                        )
                    })
                    .collect()
            });
        let Some(node) = answer.get_mut(name) else { continue };
        if let Some(asked) = asked
            && let Some(buckets) = node.get_mut("buckets").and_then(|b| b.as_array_mut())
        {
            buckets.retain(|b| {
                let pair =
                    (b.get("from").and_then(|v| v.as_f64()), b.get("to").and_then(|v| v.as_f64()));
                asked.contains(&pair)
            });
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

// a metric over a date reads instants, and says what the instant it
// arrived at is as well as the number behind it
pub(crate) fn name_date_metrics(
    store: &Store,
    targets: &[String],
    request: &Value,
    answer: &mut Value,
) {
    let Some(reqs) = request.as_object() else { return };
    for (name, def) in reqs {
        let kind = def
            .as_object()
            .and_then(|o| {
                o.keys().map(|k| k.to_string()).find(|k| {
                    matches!(
                        k.as_str(),
                        "avg" | "min" | "max" | "sum" | "median_absolute_deviation"
                    )
                })
            })
            .unwrap_or_default();
        if !kind.is_empty() {
            let field =
                def.pointer(&format!("/{kind}/field")).and_then(|f| f.as_str()).unwrap_or("");
            let ty = targets
                .iter()
                .filter_map(|n| store.get(n))
                .find_map(|st| st.read().mapping.type_of(field).map(|t| t.to_string()));
            if matches!(ty.as_deref(), Some("date") | Some("date_nanos"))
                && let Some(v) = answer.pointer(&format!("/{name}/value")).and_then(|v| v.as_f64())
            {
                let millis =
                    if ty.as_deref() == Some("date_nanos") { (v / 1e6) as i64 } else { v as i64 };
                if let Some(text) = crate::store::format_millis(millis, "iso8601") {
                    answer[name.clone()]["value_as_string"] = json!(text);
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
pub(crate) fn decimal_format(pattern: &str, value: f64) -> Option<String> {
    let start = pattern.find(['#', '0'])?;
    let end = pattern.rfind(['#', '0'])? + 1;
    let (prefix, numeric, suffix) = (&pattern[..start], &pattern[start..end], &pattern[end..]);
    let decimals = match numeric.split_once('.') {
        Some((_, frac)) => frac.chars().filter(|c| *c == '0').count(),
        None => 0,
    };
    Some(format!("{prefix}{value:.decimals$}{suffix}"))
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
        if let Some(sub) = defo.get("aggs").or_else(|| defo.get("aggregations")) {
            widen_number_keys(node, sub, floating);
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

pub(crate) fn format_terms_keys(
    result: &mut Value,
    req: &Value,
    types: &std::collections::HashMap<String, String>,
) {
    let Some(reqo) = req.as_object() else { return };
    for (name, def) in reqo {
        let Some(defo) = def.as_object() else { continue };
        let Some(node) = result.get_mut(name) else { continue };

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
