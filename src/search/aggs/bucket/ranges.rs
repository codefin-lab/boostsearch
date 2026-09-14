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
    // A bound is shown in the format the request or the mapping names, as
    // the reference shows it; it was always written in ISO form, so a
    // `format: yyyy-MM-dd` range was keyed `*-2026-03-10T00:00:00.000Z`
    // where the reference keys it `*-2026-03-10`.
    let shown_format = format.clone().unwrap_or_else(|| "strict_date_optional_time".to_string());
    let iso = |v: &Value| millis(v).and_then(|ms| crate::store::format_millis(ms, &shown_format));
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
            // the reference reports a bound as a double
            b["from"] = json!(ms as f64);
            if let Some(s) = iso(f) {
                b["from_as_string"] = json!(s);
            }
        }
        if let Some(t) = to.as_ref()
            && let Some(ms) = millis(t)
        {
            b["to"] = json!(ms as f64);
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
            // keyed, the name is the key: a bucket does not carry it again
            if let Some(bo) = b.as_object_mut() {
                bo.remove("key");
            }
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

/// A `histogram` or a `range` over an ordinary field, run through BoostCore
/// from here rather than as part of the whole request.
///
/// It lands here when something under it is run a bucket at a time, which
/// `filtered_count` takes care of, or when it names a `missing` value, which
/// BoostCore does not read. Those were answered as a histogram over a range
/// field and as a `filters` with no filters, which is to say with no buckets
/// and with a refusal. A document with no value stands in with the `missing`
/// one, so it belongs to whichever bucket that value falls in: the bucket is
/// counted again over its own documents and those, and a histogram gains the
/// bucket if it had none there, with the empty ones between.
pub(crate) fn run_native_bucket_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    name: &str,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let kind = if def.get("histogram").is_some() { "histogram" } else { "range" };
    let spec = def.get(kind).cloned().unwrap_or_else(|| json!({}));
    let missing = spec.get("missing").and_then(|m| m.as_f64());
    let mut native = def.clone();
    if let Some(o) = native.get_mut(kind).and_then(|s| s.as_object_mut()) {
        o.remove("missing");
    }
    // the keys and counts are BoostCore's; only the name is needed to find them
    let label = if name.is_empty() { "__native" } else { name };
    let query = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
    let request = json!({ label: native });
    let (_, res) = filtered_count(store, targets, &query, &Some(request))?;
    let mut answer = res
        .and_then(|mut r| r.get_mut(label).map(|v| v.take()))
        .unwrap_or_else(|| json!({"buckets": []}));
    let Some(stand_in) = missing else { return Ok(answer) };
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or_default().to_string();
    let absent = json!({"bool": {"must_not": [{"exists": {"field": field}}]}});
    let (without, _) = filtered_count(store, targets, &combine(main_query, Some(absent)), &None)?;
    if without == 0 {
        return Ok(answer);
    }
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let recount = |bucket: &mut Value| -> std::result::Result<(), Response> {
        let Some(filter) = bucket_filter(store, targets, def, bucket) else { return Ok(()) };
        let narrowed = combine(main_query, Some(filter));
        let (count, sub) = count_with_sub_aggs(store, targets, &narrowed, &sub_aggs, false)?;
        bucket["doc_count"] = json!(count);
        if let Some(Value::Object(o)) = sub {
            for (k, v) in o {
                bucket[k] = v;
            }
        }
        Ok(())
    };
    if kind == "range" {
        if let Some(buckets) = answer.get_mut("buckets").and_then(|b| b.as_array_mut()) {
            for b in buckets.iter_mut() {
                let from = b.get("from").and_then(|v| v.as_f64());
                let to = b.get("to").and_then(|v| v.as_f64());
                if from.map(|f| stand_in >= f).unwrap_or(true)
                    && to.map(|t| stand_in < t).unwrap_or(true)
                {
                    recount(b)?;
                }
            }
        }
        return Ok(answer);
    }
    let interval = spec.get("interval").and_then(|v| v.as_f64()).unwrap_or(1.0);
    let offset = spec.get("offset").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let min_doc_count = spec.get("min_doc_count").and_then(|v| v.as_u64()).unwrap_or(0);
    let key = ((stand_in - offset) / interval).floor() * interval + offset;
    let Some(buckets) = answer.get_mut("buckets").and_then(|b| b.as_array_mut()) else {
        return Ok(answer);
    };
    let mut keys: Vec<f64> =
        buckets.iter().filter_map(|b| b.get("key").and_then(|k| k.as_f64())).collect();
    if !keys.contains(&key) {
        // the buckets between the new one and the rest are there too, empty,
        // where a histogram shows its empty buckets
        let mut wanted = vec![key];
        if min_doc_count == 0
            && let (Some(lo), Some(hi)) = (keys.first().copied(), keys.last().copied())
        {
            let mut at = key + interval;
            while at < lo {
                wanted.push(at);
                at += interval;
            }
            let mut at = key - interval;
            while at > hi {
                wanted.push(at);
                at -= interval;
            }
        }
        for k in wanted {
            buckets.push(json!({"key": k, "doc_count": 0}));
            keys.push(k);
        }
        buckets.sort_by(|a, b| {
            let k = |v: &Value| v.get("key").and_then(|k| k.as_f64()).unwrap_or(0.0);
            k(a).total_cmp(&k(b))
        });
    }
    for b in buckets.iter_mut() {
        let here = b.get("key").and_then(|k| k.as_f64());
        let empty = b.get("doc_count").and_then(|c| c.as_u64()) == Some(0);
        if here == Some(key) || empty {
            recount(b)?;
        }
    }
    Ok(answer)
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

/// A bucket of a variable-width histogram as a shard hands it on: where its
/// values centre, the least and most of them, how many documents it counted,
/// and which documents those were.
#[derive(Clone)]
struct WidthBucket {
    centroid: f64,
    min: f64,
    max: f64,
    doc_count: u64,
    docs: Vec<String>,
}

/// One shard's clustering, the way `VariableWidthHistogramAggregator` does it.
///
/// The first `initial_buffer` values are held and then cut into `shard_size`
/// three quarters equal runs; every later value joins the cluster whose centre
/// is nearest, unless it is further than twice the average distance between
/// centres and there is room for another cluster, in which case it starts one.
/// A document is counted in the bucket its value joined -- except that the
/// buffered ones are handed to buckets by their place in the buffer rather
/// than by their place in the sorted run, which is what the reference's merge
/// map does, and is where their sub-aggregations are counted too.
fn cluster_one_shard(
    docs: &[(String, Vec<Vec<Held>>)],
    missing: Option<f64>,
    shard_size: usize,
    buffer_limit: usize,
) -> Vec<WidthBucket> {
    let mut counts: Vec<u64> = Vec::new();
    let mut members: Vec<Vec<String>> = Vec::new();
    let collect = |counts: &mut Vec<u64>, members: &mut Vec<Vec<String>>, ord: usize, id: &str| {
        if counts.len() <= ord {
            counts.resize(ord + 1, 0);
            members.resize(ord + 1, Vec::new());
        }
        counts[ord] += 1;
        members[ord].push(id.to_string());
    };
    // the buckets move as clusters are merged or put in order; the counts and
    // the documents counted in them move with them
    let remap = |counts: &mut Vec<u64>,
                 members: &mut Vec<Vec<String>>,
                 len: usize,
                 to: &dyn Fn(usize) -> usize| {
        let mut new_counts = vec![0u64; len];
        let mut new_members = vec![Vec::new(); len];
        for i in 0..counts.len() {
            if counts[i] == 0 {
                continue;
            }
            let dest = to(i);
            if dest >= len {
                continue;
            }
            new_counts[dest] += counts[i];
            new_members[dest].append(&mut members[i]);
        }
        *counts = new_counts;
        *members = new_members;
    };
    let mut buffer: Vec<f64> = Vec::new();
    let mut merging = false;
    let (mut mins, mut maxes, mut centroids, mut sizes): (Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>) =
        Default::default();
    let mut avg_distance = 0.0f64;
    let update_avg = |centroids: &[f64]| {
        let n = centroids.len();
        (centroids[n - 1] - centroids[0]) / (n as f64 - 1.0)
    };
    // the buffered values cut into equal runs by value, the first time the
    // buffer is full or when collection ends before it is
    let start_merging = |buffer: &[f64],
                         counts: &mut Vec<u64>,
                         members: &mut Vec<Vec<String>>,
                         mins: &mut Vec<f64>,
                         maxes: &mut Vec<f64>,
                         centroids: &mut Vec<f64>,
                         sizes: &mut Vec<f64>|
     -> f64 {
        let num_buckets = shard_size * 3 / 4;
        let mut order: Vec<usize> = (0..buffer.len()).collect();
        order.sort_by(|a, b| buffer[*a].total_cmp(&buffer[*b]));
        let per = (buffer.len() as f64 / num_buckets as f64).ceil() as usize;
        let mut merge_map = vec![0usize; buffer.len()];
        let mut ord = 0usize;
        for i in 0..order.len() {
            let val = buffer[order[i]];
            merge_map[i] = order[i] / per;
            if ord == centroids.len() {
                mins.push(val);
                maxes.push(val);
                centroids.push(val);
                sizes.push(1.0);
            } else {
                maxes[ord] = maxes[ord].max(val);
                mins[ord] = mins[ord].min(val);
                centroids[ord] = (centroids[ord] * sizes[ord] + val) / (sizes[ord] + 1.0);
                sizes[ord] += 1.0;
            }
            if (i + 1) % per == 0 {
                ord += 1;
            }
        }
        let len = ord + 1;
        let map = merge_map.clone();
        remap(counts, members, len, &|i| map.get(i).copied().unwrap_or(usize::MAX));
        if buffer.len() > 1 { update_avg(centroids) } else { 0.0 }
    };
    for (id, values) in docs {
        let mut held: Vec<f64> = values[0]
            .iter()
            .filter_map(|h| match h {
                Held::Number(n) => Some(*n),
                Held::Text(_) => None,
            })
            .collect();
        if held.is_empty()
            && let Some(m) = missing
        {
            held.push(m);
        }
        held.sort_by(|a, b| a.total_cmp(b));
        let mut previous = f64::NEG_INFINITY;
        for val in held {
            if val == previous {
                continue;
            }
            previous = val;
            if !merging {
                if buffer.len() < buffer_limit {
                    let ord = buffer.len();
                    buffer.push(val);
                    collect(&mut counts, &mut members, ord, id);
                }
                if buffer.len() == buffer_limit {
                    avg_distance = start_merging(
                        &buffer,
                        &mut counts,
                        &mut members,
                        &mut mins,
                        &mut maxes,
                        &mut centroids,
                        &mut sizes,
                    );
                    merging = true;
                }
                continue;
            }
            let mut ord = nearest(&centroids, val);
            let distance = (centroids[ord] - val).abs();
            if distance > 2.0 * avg_distance && centroids.len() < shard_size {
                mins.push(val);
                maxes.push(val);
                centroids.push(val);
                sizes.push(1.0);
                let last = centroids.len() - 1;
                collect(&mut counts, &mut members, last, id);
                if val > centroids[ord] {
                    ord += 1;
                }
                if ord != last {
                    for list in [&mut mins, &mut maxes, &mut centroids, &mut sizes] {
                        let moved = list.remove(last);
                        list.insert(ord, moved);
                    }
                    let n = centroids.len();
                    remap(&mut counts, &mut members, n, &|i| {
                        if i < ord {
                            i
                        } else if i == n - 1 {
                            ord
                        } else {
                            i + 1
                        }
                    });
                }
                avg_distance = update_avg(&centroids);
            } else {
                maxes[ord] = maxes[ord].max(val);
                mins[ord] = mins[ord].min(val);
                centroids[ord] = (centroids[ord] * sizes[ord] + val) / (sizes[ord] + 1.0);
                sizes[ord] += 1.0;
                collect(&mut counts, &mut members, ord, id);
                if ord == 0 || ord == centroids.len() - 1 {
                    avg_distance = update_avg(&centroids);
                }
            }
        }
    }
    if !merging {
        start_merging(
            &buffer,
            &mut counts,
            &mut members,
            &mut mins,
            &mut maxes,
            &mut centroids,
            &mut sizes,
        );
    }
    let mut out: Vec<WidthBucket> = (0..centroids.len())
        .map(|i| WidthBucket {
            centroid: centroids[i],
            min: mins[i],
            max: maxes[i],
            doc_count: counts.get(i).copied().unwrap_or(0),
            docs: members.get(i).cloned().unwrap_or_default(),
        })
        .collect();
    out.sort_by(|a, b| a.centroid.total_cmp(&b.centroid));
    out
}

/// The centre nearest a value, found the way the reference's binary search
/// finds it, which settles a tie between two centres its own way.
fn nearest(centroids: &[f64], value: f64) -> usize {
    let compare = |i: usize| centroids[i].total_cmp(&value);
    let closest = |a: usize, b: usize| {
        if (centroids[a] - value).abs() < (centroids[b] - value).abs() { a } else { b }
    };
    let (mut from, mut to) = (0isize, centroids.len() as isize - 1);
    while from < to {
        let mid = ((from + to) as usize) >> 1;
        match compare(mid) {
            Ordering::Equal => return mid,
            Ordering::Less => {
                if (mid as isize) < to {
                    if compare(mid + 1) == Ordering::Greater {
                        return closest(mid, mid + 1);
                    }
                } else {
                    return mid;
                }
                from = mid as isize + 1;
            }
            Ordering::Greater => {
                if mid as isize > from {
                    if compare(mid - 1) == Ordering::Less {
                        return closest(mid, mid - 1);
                    }
                } else if mid == 0 {
                    return mid;
                }
                to = mid as isize - 1;
            }
        }
    }
    from.max(0) as usize
}

/// The reference's `reduceBucket`: the counts added, the edges widened, and
/// the centre weighted by the counts, added up in the order given.
fn merge_width_buckets(buckets: &[&WidthBucket]) -> WidthBucket {
    let mut out = WidthBucket {
        centroid: 0.0,
        min: f64::INFINITY,
        max: f64::NEG_INFINITY,
        doc_count: 0,
        docs: Vec::new(),
    };
    let mut sum = 0.0f64;
    for b in buckets {
        out.doc_count += b.doc_count;
        out.min = out.min.min(b.min);
        out.max = out.max.max(b.max);
        sum += b.doc_count as f64 * b.centroid;
        out.docs.extend(b.docs.iter().cloned());
    }
    out.centroid = sum / out.doc_count as f64;
    out
}

/// Replace each run of buckets named by a `(start, end)` pair with their merge.
fn merge_width_plan(buckets: &mut Vec<WidthBucket>, plan: &[(usize, usize)]) {
    for &(start, end) in plan.iter().rev() {
        if start == end {
            continue;
        }
        let taken: Vec<WidthBucket> = buckets.drain(start + 1..=end).rev().collect();
        let mut run: Vec<&WidthBucket> = taken.iter().collect();
        run.push(&buckets[start]);
        let merged = merge_width_buckets(&run);
        buckets[start] = merged;
    }
}

/// The reference's `reduceBuckets`: the buckets of every list taken in order
/// of their centres, the ones with the same centre reduced together, and then
/// the two nearest centres merged until as many are left as were asked for.
fn reduce_width_buckets(lists: &[Vec<WidthBucket>], want: usize) -> Vec<WidthBucket> {
    let mut all: Vec<(usize, &WidthBucket)> =
        lists.iter().enumerate().flat_map(|(s, list)| list.iter().map(move |b| (s, b))).collect();
    all.sort_by(|a, b| a.1.centroid.total_cmp(&b.1.centroid).then(a.0.cmp(&b.0)));
    let mut buckets: Vec<WidthBucket> = Vec::new();
    let mut run: Vec<&WidthBucket> = Vec::new();
    for (_, b) in all {
        if run.last().map(|r| r.centroid.total_cmp(&b.centroid) != Ordering::Equal).unwrap_or(false)
        {
            buckets.push(merge_width_buckets(&run));
            run.clear();
        }
        run.push(b);
    }
    if !run.is_empty() {
        buckets.push(merge_width_buckets(&run));
    }
    let mut ranges: Vec<(usize, usize, f64, u64)> =
        buckets.iter().enumerate().map(|(i, b)| (i, i, b.centroid, b.doc_count)).collect();
    while ranges.len() > want {
        let mut closest = 0usize;
        let mut smallest = f64::INFINITY;
        for i in 0..ranges.len() - 1 {
            let distance = ranges[i + 1].2 - ranges[i].2;
            if distance < smallest {
                closest = i;
                smallest = distance;
            }
        }
        let next = ranges.remove(closest + 1);
        let here = &mut ranges[closest];
        here.0 = here.0.min(next.0);
        here.1 = here.1.max(next.1);
        if here.3 + next.3 > 0 {
            here.2 = (here.2 * here.3 as f64 + next.2 * next.3 as f64) / (here.3 + next.3) as f64;
            here.3 += next.3;
        }
    }
    let plan: Vec<(usize, usize)> = ranges.iter().map(|r| (r.0, r.1)).collect();
    merge_width_plan(&mut buckets, &plan);
    buckets
}

/// `variable_width_histogram`: buckets whose edges follow the data.
///
/// A port of OpenSearch's aggregator. Each shard clusters its own values in
/// the order it holds them (see `cluster_one_shard`), and the shards' buckets
/// are then merged: the ones with the same centre first, then the two nearest
/// centres again and again until `buckets` are left, then the ones that start
/// at the same value, and last any two that overlap are split at the middle of
/// the overlap. The sorted values were cut at their widest gaps instead, which
/// found different buckets: five over a price came back as 2652, 137, 1, 134
/// and 80 documents where the reference answers 2654, 133, 136, 34 and 47.
pub(crate) fn run_variable_width_histogram(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    name: &str,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("variable_width_histogram").cloned().unwrap_or(json!({}));
    let want = spec.get("buckets").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
    // the reference checks these where it builds the aggregation, on the shard
    let index = targets.first().cloned().unwrap_or_default();
    let bad = |reason: String| {
        crate::search::search_shard_failure("illegal_argument_exception", &reason, &index)
    };
    if want == 0 {
        return Err(bad(format!("[buckets] must be greater than 0 for [{name}]")));
    }
    let shard_size = match spec.get("shard_size").and_then(|v| v.as_u64()) {
        Some(s) if s <= 1 => {
            return Err(bad(format!("[shard_size] must be greater than 1 for [{name}]")));
        }
        Some(s) => s as usize,
        None => want * 50,
    };
    let initial_buffer = match spec.get("initial_buffer").and_then(|v| v.as_u64()) {
        Some(0) => {
            return Err(bad(format!("[initial_buffer] must be greater than 0 for [{name}]")));
        }
        Some(b) => b as usize,
        None => (10 * shard_size).min(50_000),
    };
    if initial_buffer < want {
        return Err(bad(format!(
            "initial_buffer must be at least buckets but was [{initial_buffer}<{want}] for [{name}]"
        )));
    }
    if shard_size * 3 / 4 < want {
        return Err(bad(format!(
            "3/4 of shard_size must be at least buckets but was [{}<{want}] for [{name}]",
            shard_size * 3 / 4
        )));
    }
    let (field, missing) = agg_field_and_missing(&spec);
    let query = combine(main_query, None);
    let shards = shard_docs(store, targets, &query, &[&field])?;
    let per_shard: Vec<Vec<WidthBucket>> = shards
        .iter()
        .map(|s| cluster_one_shard(&s.docs, missing, shard_size, initial_buffer))
        .collect();

    // each shard reduces its own buckets before handing them on -- the
    // reference collects a shard in slices and reduces the slices -- and the
    // coordinator reduces what the shards handed on
    let reduced: Vec<Vec<WidthBucket>> =
        per_shard.into_iter().map(|list| reduce_width_buckets(&[list], want)).collect();
    let mut buckets = reduce_width_buckets(&reduced, want);

    // then the ones that begin at the same value
    let mut plan: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < buckets.len() {
        let mut end = i;
        while end + 1 < buckets.len() && buckets[end + 1].min == buckets[i].min {
            end += 1;
        }
        plan.push((i, end));
        i = end + 1;
    }
    merge_width_plan(&mut buckets, &plan);

    // and two that overlap meet halfway
    for i in 1..buckets.len() {
        if buckets[i].min < buckets[i - 1].max {
            buckets[i].min = (buckets[i - 1].max + buckets[i].min) / 2.0;
            buckets[i - 1].max = buckets[i].min;
        }
    }

    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let format = spec.get("format").and_then(|f| f.as_str()).map(|s| s.to_string());
    let mut out = Vec::new();
    for b in &buckets {
        let mut bucket =
            json!({"min": b.min, "key": b.centroid, "max": b.max, "doc_count": b.doc_count});
        if let Some(pattern) = format.as_deref() {
            for (k, v) in [("min", b.min), ("key", b.centroid), ("max", b.max)] {
                if let Some(text) = decimal_format(pattern, v) {
                    bucket[format!("{k}_as_string")] = json!(text);
                }
            }
        }
        if sub_aggs.is_some() {
            let narrowed =
                json!({"bool": {"filter": [query.clone(), {"ids": {"values": b.docs}}]}});
            let (_, sub) = count_with_sub_aggs(store, targets, &narrowed, &sub_aggs, false)?;
            if let Some(Value::Object(o)) = sub {
                for (k, v) in o {
                    bucket[k] = v;
                }
            }
        }
        out.push(bucket);
    }
    Ok(json!({"buckets": out}))
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
