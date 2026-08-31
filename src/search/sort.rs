//! Ranking: the keys a request sorts by, and how candidates compare.

use super::*;

pub(crate) fn parse_sort(spec: Option<&Value>) -> Vec<SortKey> {
    let Some(spec) = spec else { return vec![] };
    let items: Vec<Value> = match spec {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    };
    let mut out = Vec::new();
    for item in items {
        match item {
            Value::String(f) => out.push(SortKey {
                field: f,
                desc: false,
                mode: None,
                missing_last: true,
                nested: None,
                nested_filter: None,
            }),
            Value::Object(o) => {
                for (field, opts) in o {
                    let desc = match &opts {
                        Value::String(s) => s.eq_ignore_ascii_case("desc"),
                        Value::Object(oo) => oo
                            .get("order")
                            .and_then(|v| v.as_str())
                            .map(|s| s.eq_ignore_ascii_case("desc"))
                            .unwrap_or(false),
                        _ => false,
                    };
                    let mode = opts
                        .get("mode")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_ascii_lowercase());
                    let missing_last = opts
                        .get("missing")
                        .and_then(|v| v.as_str())
                        .map(|m| m == "_last")
                        .unwrap_or(true);
                    // a sort may say which nested object it reads inside;
                    // without one it reads the document itself
                    let nested = opts
                        .pointer("/nested/path")
                        .or_else(|| opts.get("nested_path"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    // a nested sort may take only the objects that match a
                    // filter, and a document whose objects all fail it has no
                    // value to sort by at all
                    let nested_filter = opts
                        .pointer("/nested/filter")
                        .or_else(|| opts.get("nested_filter"))
                        .cloned();
                    out.push(SortKey {
                        field,
                        desc,
                        mode,
                        missing_last,
                        nested,
                        nested_filter,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// Decode one raw columnar u64 into the value its column type really holds.
pub(crate) fn decode_col_value(raw: u64, ty: boostcore::columnar::ColumnType) -> Option<SortValue> {
    use boostcore::columnar::ColumnType;
    match ty {
        ColumnType::I64 | ColumnType::DateTime => Some(SortValue::I64(
            boostcore::columnar::MonotonicallyMappableToU64::from_u64(raw),
        )),
        ColumnType::F64 => Some(SortValue::F64(
            <f64 as boostcore::columnar::MonotonicallyMappableToU64>::from_u64(raw),
        )),
        ColumnType::U64 => Some(SortValue::U64(raw)),
        ColumnType::Bool => Some(SortValue::I64(raw as i64)),
        ColumnType::Str | ColumnType::Bytes | ColumnType::IpAddr => None,
    }
}

/// Collapse a document's multiple values for one sort field.
///
/// OpenSearch does this with Java `long` arithmetic, so `sum` and `avg` wrap on
/// overflow -- `[i64::MAX, 1]` really does sort as a large negative number --
/// while `median` averages the two middle values in floating point.
pub(crate) fn reduce_sort_values(vals: &mut Vec<SortValue>, mode: &str) -> SortValue {
    let n = vals.len();
    match mode {
        "max" | "min" => {
            vals.sort_by(|a, b| a.cmp_asc(b));
            if mode == "max" { vals[n - 1].clone() } else { vals[0].clone() }
        }
        "sum" | "avg" => {
            // doc values arrive sorted in OpenSearch, so summing in ascending
            // order is what reproduces its floating-point result exactly
            vals.sort_by(|a, b| a.cmp_asc(b));
            let is_float = vals.iter().any(|v| matches!(v, SortValue::F64(_)));
            if is_float {
                let total: f64 = vals.iter().filter_map(|v| v.as_f64()).sum();
                let out = if mode == "sum" { total } else { total / n as f64 };
                SortValue::F64(out)
            } else if vals.iter().any(|v| matches!(v, SortValue::U64(_))) {
                // one unsigned value makes the whole field unsigned
                let total = vals.iter().fold(0u64, |acc, v| match v {
                    SortValue::U64(x) => acc.wrapping_add(*x),
                    SortValue::I64(x) => acc.wrapping_add(*x as u64),
                    _ => acc,
                });
                if mode == "sum" {
                    SortValue::U64(total)
                } else {
                    // unsigned_long keeps exact integer arithmetic, rounding half up
                    SortValue::U64((total + (n as u64) / 2) / n as u64)
                }
            } else {
                let total = vals.iter().fold(0i64, |acc, v| match v {
                    SortValue::I64(x) => acc.wrapping_add(*x),
                    SortValue::U64(x) => acc.wrapping_add(*x as i64),
                    _ => acc,
                });
                if mode == "sum" {
                    SortValue::I64(total)
                } else {
                    SortValue::I64((total as f64 / n as f64).round() as i64)
                }
            }
        }
        "median" => {
            vals.sort_by(|a, b| a.cmp_asc(b));
            if n % 2 == 1 {
                return vals[n / 2].clone();
            }
            if vals.iter().any(|v| matches!(v, SortValue::U64(_))) {
                // same exact, wrapping arithmetic the unsigned_long avg uses
                let as_u64 = |v: &SortValue| match v {
                    SortValue::U64(x) => *x,
                    SortValue::I64(x) => *x as u64,
                    other => other.as_f64().unwrap_or(0.0) as u64,
                };
                let lo = as_u64(&vals[n / 2 - 1]);
                let hi = as_u64(&vals[n / 2]);
                return SortValue::U64((lo.wrapping_add(hi) + 1) / 2);
            }
            let lo = vals[n / 2 - 1].as_f64().unwrap_or(0.0);
            let hi = vals[n / 2].as_f64().unwrap_or(0.0);
            let avg = (lo + hi) / 2.0;
            if vals.iter().any(|v| matches!(v, SortValue::F64(_))) {
                return SortValue::F64(avg);
            }
            // integer fields round half up, the way Java's Math.round does
            let rounded = (avg + 0.5).floor();
            if rounded >= 0.0 && vals.iter().any(|v| matches!(v, SortValue::U64(_))) {
                SortValue::U64(rounded as u64)
            } else {
                SortValue::I64(rounded as i64)
            }
        }
        _ => vals[0].clone(),
    }
}

pub(crate) fn cmp_sorted(a: &[SortValue], b: &[SortValue], desc: &[bool]) -> Ordering {
    for (i, d) in desc.iter().enumerate() {
        let ord = a[i].cmp_asc(&b[i]);
        let ord = if *d && ord != Ordering::Equal { ord.reverse() } else { ord };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

pub(crate) fn prune_by(buf: &mut Vec<Cand>, limit: usize, desc: &[bool]) {
    if limit == 0 || buf.len() <= limit {
        return;
    }
    buf.select_nth_unstable_by(limit - 1, |x, y| cmp_sorted(&x.sort, &y.sort, desc));
    buf.truncate(limit);
}

/// Which kind of date a sort key names, if it names one at all: `Some(false)`
/// for a `date`, `Some(true)` for a `date_nanos`.
pub(crate) fn date_sort_kind(store: &Store, targets: &[String], field: &str) -> Option<bool> {
    targets
        .iter()
        .filter_map(|n| store.get(n))
        .find_map(|st| match st.read().mapping.type_of(field) {
            Some("date") => Some(false),
            Some("date_nanos") => Some(true),
            _ => None,
        })
}

/// Read one `search_after` element back into the value the sort produced.
pub(crate) fn sort_value_from_json(v: &Value, date: Option<bool>) -> SortValue {
    match v {
        // a marker for a date is written in the unit that field reports in,
        // and the column counts nanoseconds either way
        Value::Number(n) => match date {
            // a marker is written in the unit the field reports in, which is
            // the unit the values are compared in
            Some(_) => SortValue::I64(n.as_i64().unwrap_or(0)),
            None => SortValue::F64(n.as_f64().unwrap_or(0.0)),
        },
        // a marker for a date field may be written as a date rather than as
        // the number the column holds
        Value::String(s) if date.is_some() => crate::store::canonical_date(v)
            .and_then(|d| crate::store::parse_date_lenient(&d))
            .map(|d| {
                let nanos = d.unix_timestamp_nanos() as i64;
                SortValue::I64(if date == Some(true) { nanos } else { nanos / 1_000_000 })
            })
            .unwrap_or_else(|| SortValue::Str(s.clone())),
        Value::String(s) => SortValue::Str(s.clone()),
        Value::Null => SortValue::Missing,
        other => SortValue::Str(other.to_string()),
    }
}

/// Read each candidate's arrival order out of the index.
///
/// The writer spreads one bulk request across its worker threads, so which
/// segment a document lands in -- and what doc id it gets there -- does not
/// follow the order it was sent in. `_seq` does, and reading it here rather
/// than while collecting keeps it off the path every matching document walks:
/// by now the field is only read for the handful of candidates that survived.
pub(crate) fn fill_seq(
    cands: &mut [Cand],
    searchers: &[(String, Searcher, std::sync::Arc<parking_lot::RwLock<IdxState>>)],
) {
    let mut cols: std::collections::HashMap<(usize, u32), Option<boostcore::columnar::Column<u64>>> =
        std::collections::HashMap::new();
    for c in cands.iter_mut() {
        let (shard, seg) = (c.shard, c.addr.segment_ord);
        let col = cols.entry((shard, seg)).or_insert_with(|| {
            let (_, searcher, _) = searchers.get(shard)?;
            let reader = searcher.segment_readers().get(seg as usize)?;
            reader.fast_fields().u64("_seq").ok()
        });
        if let Some(col) = col {
            c.seq = col.first(c.addr.doc_id).unwrap_or(u64::MAX);
        }
    }
}

pub(crate) fn cmp_cands(a: &Cand, b: &Cand, sort_keys: &[SortKey]) -> Ordering {
    // ties fall back to document order, which is insertion order within a
    // shard -- otherwise equally-scored hits come back in a different order
    // from one run to the next
    let by_doc = || {
        a.shard
            .cmp(&b.shard)
            .then(a.seq.cmp(&b.seq))
            .then(a.addr.segment_ord.cmp(&b.addr.segment_ord))
            .then(a.addr.doc_id.cmp(&b.addr.doc_id))
    };
    if sort_keys.is_empty() {
        return b
            .score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(by_doc);
    }
    for (i, k) in sort_keys.iter().enumerate() {
        let ord = cmp_with_missing(&a.sort[i], &b.sort[i], k);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    by_doc()
}

/// Compare two values of one sort key, in the direction it asks for.
///
/// A document with no value goes where `missing` says, which is last in the
/// order the caller sees -- so it stays last when the sort is reversed rather
/// than moving to the front with everything else.
pub(crate) fn cmp_with_missing(a: &SortValue, b: &SortValue, k: &SortKey) -> Ordering {
    let missing = |v: &SortValue| matches!(v, SortValue::Missing);
    if missing(a) || missing(b) {
        let last = if k.missing_last { Ordering::Greater } else { Ordering::Less };
        return match (missing(a), missing(b)) {
            (true, true) => Ordering::Equal,
            (true, false) => last,
            (false, true) => last.reverse(),
            _ => unreachable!(),
        };
    }
    let ord = a.cmp_asc(b);
    if k.desc && ord != Ordering::Equal { ord.reverse() } else { ord }
}

/// Keep only the best `want` candidates, pruning in amortised linear time
/// rather than sorting every match.
pub(crate) fn prune(cands: &mut Vec<Cand>, want: usize, sort_keys: &[SortKey]) {
    if want == 0 || cands.len() <= want {
        return;
    }
    cands.select_nth_unstable_by(want - 1, |a, b| cmp_cands(a, b, sort_keys));
    cands.truncate(want);
}

/// Settle a sort that only counts some of a document's nested objects.
///
/// Where a sort names a filter on the objects it reads, only the objects that
/// match it have anything to say; a document whose objects all fail the filter
/// has no value at all, and sorts with the missing ones.
pub(crate) fn sort_by_filtered_nested(
    store: &Store,
    targets: &[String],
    cands: &mut [Cand],
    searchers: &Searchers,
    sort_keys: &[SortKey],
) {

    for (i, key) in sort_keys.iter().enumerate() {
        let (Some(path), Some(filter)) = (key.nested.as_ref(), key.nested_filter.as_ref())
        else {
            continue;
        };
        let leaf = key.field.strip_prefix(&format!("{path}.")).unwrap_or(&key.field);
        for c in cands.iter_mut() {
            let (_, searcher, st) = &searchers[c.shard];
            let g = st.read();
            let Some((_, src)) = source_of(searcher, &g, c.addr) else { continue };
            let objects: Vec<Value> = match src.pointer(&format!("/{}", path.replace('.', "/")))
            {
                Some(Value::Array(a)) => a.clone(),
                Some(other) => vec![other.clone()],
                None => Vec::new(),
            };
            let mut values: Vec<f64> = Vec::new();
            for object in objects {
                if !object_matches(filter, &object, path) {
                    continue;
                }
                if let Some(v) = object.pointer(&format!("/{}", leaf.replace('.', "/"))) {
                    if let Some(n) = number_of(v) {
                        values.push(n);
                    }
                }
            }
            // the values are read as instants; a `date` is reported in
            // milliseconds, which is the unit it is compared in
            let nanos_kept = targets
                .iter()
                .filter_map(|n| store.get(n))
                .find_map(|st| st.read().mapping.type_of(&key.field).map(|t| t.to_string()))
                .map(|t| t != "date")
                .unwrap_or(true);
            let values: Vec<f64> = if nanos_kept {
                values
            } else {
                values.into_iter().map(|v| (v / 1e6).trunc()).collect()
            };
            let picked = match key.mode.as_deref() {
                Some("max") => values.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
                Some("sum") => values.iter().sum(),
                Some("avg") => {
                    values.iter().sum::<f64>() / (values.len().max(1) as f64)
                }
                _ => values.iter().cloned().fold(f64::INFINITY, f64::min),
            };
            if let Some(slot) = c.sort.get_mut(i) {
                *slot = if values.is_empty() {
                    SortValue::Missing
                } else {
                    SortValue::F64(picked)
                };
            }
        }
    }
    
}
