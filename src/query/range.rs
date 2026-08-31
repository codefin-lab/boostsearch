//! Ranges over the numbers, dates and addresses an index holds.

use super::*;

/// A `*_range` field stores an interval per document, so a range query over it
/// compares two intervals rather than a value against bounds. The stored
/// endpoints are already separate numeric paths, so each relation is a pair of
/// ordinary range queries.
pub(crate) fn build_range_field_query(
    ctx: &Ctx,
    field: &str,
    spec: &Value,
) -> Option<Result<Box<dyn Query>>> {
    let kind = ctx.mapping.type_of(field)?;
    if !kind.ends_with("_range") {
        return None;
    }
    let relation = spec
        .get("relation")
        .and_then(|v| v.as_str())
        .unwrap_or("intersects")
        .to_ascii_lowercase();
    // a date bound may be written as date math, which names a whole unit; the
    // bound decides which end of it is meant
    let bound = |inclusive_key: &str, exclusive_key: &str, up_when_inclusive: bool| {
        let (v, inclusive) = match spec.get(inclusive_key) {
            Some(v) => (v.clone(), true),
            None => (spec.get(exclusive_key)?.clone(), false),
        };
        if !kind.starts_with("date") {
            return Some((v, inclusive));
        }
        let up = if inclusive { up_when_inclusive } else { !up_when_inclusive };
        // the endpoints a date_range stores are numbers, the way a date is
        let rewritten = crate::store::date_number_bound(&v, up, None, false)
            .map(|n| Value::Number(n.into()))
            .unwrap_or(v);
        Some((rewritten, inclusive))
    };
    let q_lo = bound("gte", "gt", false);
    let q_hi = bound("lte", "lt", true);
    // an exclusive query bound stays exclusive in the comparison it becomes:
    // a bucket ending `lt` March does not reach an interval starting on the 1st
    let lower_key = |inclusive: bool| if inclusive { "gte" } else { "gt" };
    let upper_key = |inclusive: bool| if inclusive { "lte" } else { "lt" };
    let lo_field = format!("{field}.gte");
    let hi_field = format!("{field}.lte");

    let mut clauses: Vec<Value> = Vec::new();
    match relation.as_str() {
        // the stored interval overlaps the query interval
        "intersects" => {
            if let Some((hi, inc)) = &q_hi {
                clauses.push(serde_json::json!({"range": {lo_field.clone(): {upper_key(*inc): hi}}}));
            }
            if let Some((lo, inc)) = &q_lo {
                clauses.push(serde_json::json!({"range": {hi_field.clone(): {lower_key(*inc): lo}}}));
            }
        }
        // the stored interval covers the query interval
        "contains" => {
            if let Some((lo, _)) = &q_lo {
                clauses.push(serde_json::json!({"range": {lo_field.clone(): {"lte": lo}}}));
            }
            if let Some((hi, _)) = &q_hi {
                clauses.push(serde_json::json!({"range": {hi_field.clone(): {"gte": hi}}}));
            }
        }
        // the stored interval sits inside the query interval
        "within" => {
            if let Some((lo, inc)) = &q_lo {
                clauses.push(serde_json::json!({"range": {lo_field.clone(): {lower_key(*inc): lo}}}));
            }
            if let Some((hi, inc)) = &q_hi {
                clauses.push(serde_json::json!({"range": {hi_field.clone(): {upper_key(*inc): hi}}}));
            }
        }
        other => {
            return Some(Err(anyhow!("unsupported range relation [{other}]")));
        }
    }
    if clauses.is_empty() {
        // no bounds: every document that has the field at all
        clauses.push(serde_json::json!({"exists": {"field": lo_field}}));
    }
    let combined = serde_json::json!({"bool": {"filter": clauses}});
    Some(build(ctx, &combined))
}

pub(crate) fn build_range(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
    let (field, spec) = single_key(body)?;
    if let Some(r) = build_range_field_query(ctx, &field, &spec) {
        return r;
    }
    let (f, path, _) = ctx.resolve(&field, false);
    let get = |keys: [&str; 2]| -> Option<(Value, bool)> {
        for (i, k) in keys.iter().enumerate() {
            if let Some(v) = spec.get(*k) {
                if !v.is_null() {
                    return Some((v.clone(), i == 0));
                }
            }
        }
        None
    };
    // `from`/`to` are the older spelling, with inclusivity as its own flag
    let older = |key: &str, flag: &str| -> Option<(Value, bool)> {
        let v = spec.get(key).filter(|v| !v.is_null())?.clone();
        let inclusive = match spec.get(flag) {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => s != "false",
            _ => true,
        };
        Some((v, inclusive))
    };
    let mut lower = get(["gte", "gt"]).or_else(|| older("from", "include_lower"));
    let mut upper = get(["lte", "lt"]).or_else(|| older("to", "include_upper"));
    // OpenSearch's default date format accepts a bare year; our date values are
    // indexed as ISO strings, which compare correctly lexicographically

    // a value gathered under a flat_object was stored in its canonical
    // spelling, and a bound has to be written the same way to compare with it
    let under_flat = {
        let mut walked = String::new();
        let mut found = false;
        for part in field.split('.') {
            walked = if walked.is_empty() {
                part.to_string()
            } else {
                format!("{walked}.{part}")
            };
            if ctx.mapping.type_of(&walked) == Some("flat_object") {
                found = true;
            }
        }
        found
    };
    // A date under a flat_object was stored in its canonical spelling, so a
    // bound written the short way has to be canonicalised to compare with it.
    // But "2.1" is a version, not a year: rewriting the bound would throw the
    // text away, so the canonical spelling is asked for as well, not instead.
    let flat_bounds: Option<(Option<(Value, bool)>, Option<(Value, bool)>)> = under_flat
        .then(|| {
            let canon = |b: &Option<(Value, bool)>| -> Option<(Value, bool)> {
                let (Value::String(text), inclusive) = b.clone()? else { return None };
                let iso = crate::store::canonical_date(&Value::String(text.clone()))?;
                (iso != text).then_some((Value::String(iso), inclusive))
            };
            (canon(&lower), canon(&upper))
        })
        .filter(|(lo, hi)| lo.is_some() || hi.is_some());
    if matches!(ctx.mapping.type_of(&field), Some("ip" | "date" | "date_nanos")) {
        let ty = ctx.mapping.type_of(&field).unwrap_or_default().to_string();
        // a range query may name the format its bounds are written in, which
        // stands in for the one the mapping declares
        let mapped = ctx.mapping.field_option(&field, "format");
        let fmt = spec
            .get("format")
            .and_then(|v| v.as_str())
            .or_else(|| mapped.as_ref().and_then(|v| v.as_str()));
        for (is_lower, b) in [(true, &mut lower), (false, &mut upper)] {
            if let Some((v, inclusive)) = b.clone() {
                let up = (is_lower && !inclusive) || (!is_lower && inclusive);
                let rewritten = if ty == "ip" {
                    ip_value(ctx, &field, &v)
                } else {
                    crate::store::date_number_bound(&v, up, fmt, ty == "date_nanos")
                        .map(|n| Value::Number(n.into()))
                        .unwrap_or(v.clone())
                };
                *b = Some((rewritten, inclusive));
            }
        }
    }
    // A numeric bound against a string field is a lexicographic comparison in
    // OpenSearch -- "5" and "400" are both below 500, "ingesting..." is not.
    if matches!(
        ctx.mapping.type_of(&field),
        Some("keyword" | "text" | "wildcard" | "constant_keyword" | "search_as_you_type"
            | "match_only_text")
    ) {
        for b in [&mut lower, &mut upper] {
            if let Some((Value::Number(n), inclusive)) = b.clone() {
                *b = Some((Value::String(n.to_string()), inclusive));
            }
        }
    }
    if lower.is_none() && upper.is_none() {
        return Ok(Box::new(AllQuery));
    }

    let sample = lower.as_ref().or(upper.as_ref()).map(|(v, _)| v.clone()).unwrap_or(Value::Null);
    // there are only two booleans, so a range over them is just the set of
    // values it admits -- and range scans do not accept the type at all
    if sample.is_boolean() {
        let ok = |b: bool| {
            lower.as_ref().is_none_or(|(v, inc)| match v.as_bool() {
                Some(l) => b > l || (*inc && b == l),
                None => true,
            }) && upper.as_ref().is_none_or(|(v, inc)| match v.as_bool() {
                Some(u) => b < u || (*inc && b == u),
                None => true,
            })
        };
        let terms: Vec<Term> = [false, true]
            .into_iter()
            .filter(|b| ok(*b))
            .flat_map(|b| term_for(f, &path, &Value::Bool(b)))
            .collect();
        return Ok(any_of(terms));
    }
    let types: Vec<Type> = match &sample {
        Value::Number(n) => {
            if n.is_f64() && n.as_i64().is_none() {
                vec![Type::F64]
            } else if matches!(n.as_i64(), Some(y) if (1000..=9999).contains(&y))
                && !is_numeric_type(ctx.mapping.type_of(&field))
            {
                // a bare year may address a date field whose values are indexed
                // as ISO strings, which compare correctly as text
                vec![Type::I64, Type::U64, Type::F64, Type::Str]
            } else {
                vec![Type::I64, Type::U64, Type::F64]
            }
        }
        Value::String(s) if parse_datetime(s).is_some() => vec![Type::Date, Type::Str],
        Value::String(_) => vec![Type::Str],
        Value::Bool(_) => vec![Type::Bool],
        _ => vec![Type::Str],
    };

    // only build the typed variants this field has actually held; each extra
    // variant is a separate range scan unioned over the whole segment
    // BOOSTSEARCH_NO_KIND_NARROW=1 disables the narrowing, for A/B runs
    let narrowing_on = ctx.kinds_complete && std::env::var("BOOSTSEARCH_NO_KIND_NARROW").is_err();
    let types: Vec<Type> = match ctx.observed_kinds.get(&field).filter(|_| narrowing_on) {
        Some(&kinds) if kinds != 0 => {
            let narrowed: Vec<Type> = types
                .iter()
                .copied()
                .filter(|t| match t {
                    Type::I64 => kinds & crate::store::KIND_I64 != 0,
                    Type::U64 => kinds & crate::store::KIND_U64 != 0,
                    Type::F64 => kinds & crate::store::KIND_F64 != 0,
                    Type::Str => kinds & crate::store::KIND_STR != 0,
                    Type::Date => kinds & crate::store::KIND_DATE != 0,
                    _ => true,
                })
                .collect();
            if narrowed.is_empty() { types } else { narrowed }
        }
        _ => types,
    };

    // Whatever a value under a flat_object looked like, it was stored as text,
    // so a bound that reads as a date must still be compared as one.
    let mut types = types;
    if under_flat && !types.contains(&Type::Str) {
        types.push(Type::Str);
    }

    // A single numeric type means block statistics can drive the scan, which
    // lets whole runs of documents be skipped instead of compared one by one.
    let mut subs: Vec<Box<dyn Query>> = Vec::new();
    for t in types.iter().copied() {
        let lo = bound_term(f, &path, lower.as_ref(), t, true);
        let hi = bound_term(f, &path, upper.as_ref(), t, false);
        if matches!(lo, Bound::Unbounded) && matches!(hi, Bound::Unbounded) {
            continue;
        }
        subs.push(Box::new(RangeQuery::new(lo, hi)));
    }
    // The canonical spelling is text whatever the values were narrowed to: a
    // date under a flat_object is stored as the text of its canonical form.
    if let Some((flat_lo, flat_hi)) = &flat_bounds {
        for t in [Type::Str, Type::Date] {
            let lo = bound_term(f, &path, flat_lo.as_ref().or(lower.as_ref()), t, true);
            let hi = bound_term(f, &path, flat_hi.as_ref().or(upper.as_ref()), t, false);
            if matches!(lo, Bound::Unbounded) && matches!(hi, Bound::Unbounded) {
                continue;
            }
            subs.push(Box::new(RangeQuery::new(lo, hi)));
        }
    }
    let general: Box<dyn Query> = match subs.len() {
        0 => Box::new(EmptyQuery),
        1 => subs.into_iter().next().unwrap(),
        _ => Box::new(BooleanQuery::new(
            subs.into_iter().map(|s| (Occur::Should, s)).collect(),
        )),
    };

    // A single numeric type means block statistics can drive the scan. The query
    // itself decides per search whether that actually beats the general path.
    // the block path scans one typed range, and a flat_object bound asked for
    // two spellings of the value
    if types.len() == 1
        && flat_bounds.is_none()
        && std::env::var("BOOSTSEARCH_NO_BLOCK_RANGE").is_err()
    {
        if let Some(q) =
            block_range_query(ctx, &field, types[0], lower.as_ref(), upper.as_ref(), &general)
        {
            return Ok(q);
        }
    }
    Ok(general)
}

/// Encode a range bound into the column's monotonic u64 space.
///
/// Returns `None` whenever the bound cannot be represented exactly -- an
/// exclusive float bound, say -- so the caller falls back to the general path
/// rather than answering a slightly different question.
pub(crate) fn u64_bound(
    ty: Type,
    b: Option<&(Value, bool)>,
    is_lower: bool,
) -> Option<u64> {
    use boostcore::columnar::MonotonicallyMappableToU64;
    let Some((v, inclusive)) = b else {
        return Some(if is_lower { u64::MIN } else { u64::MAX });
    };
    let step = |x: u64| -> Option<u64> {
        if *inclusive {
            Some(x)
        } else if is_lower {
            x.checked_add(1)
        } else {
            x.checked_sub(1)
        }
    };
    match (ty, v) {
        (Type::I64, Value::Number(n)) => step(MonotonicallyMappableToU64::to_u64(n.as_i64()?)),
        (Type::U64, Value::Number(n)) => step(n.as_u64()?),
        (Type::F64, Value::Number(n)) => {
            // stepping a float bound would change which values qualify
            if !*inclusive {
                return None;
            }
            Some(MonotonicallyMappableToU64::to_u64(n.as_f64()?))
        }
        (Type::Date, Value::String(s)) => {
            let dt = parse_datetime(s)?;
            step(MonotonicallyMappableToU64::to_u64(dt))
        }
        _ => None,
    }
}

pub(crate) fn block_range_query(
    ctx: &Ctx,
    field: &str,
    ty: Type,
    lower: Option<&(Value, bool)>,
    upper: Option<&(Value, bool)>,
    general: &Box<dyn Query>,
) -> Option<Box<dyn Query>> {
    use boostcore::columnar::ColumnType;
    let column_type = match ty {
        Type::I64 => ColumnType::I64,
        Type::U64 => ColumnType::U64,
        Type::F64 => ColumnType::F64,
        Type::Date => ColumnType::DateTime,
        _ => return None,
    };
    let lo = u64_bound(ty, lower, true)?;
    let hi = u64_bound(ty, upper, false)?;
    if lo > hi {
        return Some(Box::new(EmptyQuery));
    }
    Some(Box::new(crate::blockstats::BlockRangeQuery {
        column: ctx.column_name(field, false),
        column_type,
        lo,
        hi,
        cache: ctx.stats.clone(),
        fallback: general.box_clone().into(),
    }))
}

pub(crate) fn is_numeric_type(t: Option<&str>) -> bool {
    matches!(
        t,
        Some("long") | Some("integer") | Some("short") | Some("byte") | Some("double")
            | Some("float") | Some("half_float") | Some("scaled_float") | Some("unsigned_long")
    )
}

pub(crate) fn bound_term(
    f: Field,
    path: &str,
    b: Option<&(Value, bool)>,
    ty: Type,
    _is_lower: bool,
) -> Bound<Term> {
    let Some((v, inclusive)) = b else { return Bound::Unbounded };
    let base = Term::from_field_json_path(f, path, true);
    let mut t = base;
    match (ty, v) {
        (Type::I64, Value::Number(n)) => t.append_type_and_fast_value(n.as_i64().unwrap_or(0)),
        (Type::U64, Value::Number(n)) => match n.as_u64() {
            Some(u) => t.append_type_and_fast_value(u),
            None => return Bound::Unbounded,
        },
        (Type::F64, Value::Number(n)) => t.append_type_and_fast_value(n.as_f64().unwrap_or(0.0)),
        (Type::Date, Value::String(s)) => match parse_datetime(s) {
            Some(d) => t.append_type_and_fast_value(d),
            None => return Bound::Unbounded,
        },
        (Type::Bool, Value::Bool(b)) => t.append_type_and_fast_value(*b),
        (Type::Str, Value::String(s)) => t.append_type_and_str(s),
        (Type::Str, other) => t.append_type_and_str(&other.to_string()),
        _ => return Bound::Unbounded,
    }
    if *inclusive { Bound::Included(t) } else { Bound::Excluded(t) }
}
