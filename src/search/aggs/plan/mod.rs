//! Who answers which aggregation, and what has to happen to the request
//! before VeloCore is given it.

use super::*;
use crate::search::*;

mod check;
pub(crate) use check::*;
mod rewrite;
pub(crate) use rewrite::*;

/// Whether the total can be had without walking the matches.
///
/// `Weight::count` reads the figure from the postings header for a term query
/// and from the segment for a match-all, and otherwise counts by iterating.
/// Splitting top-k from the count only pays where that shortcut exists: where
/// it does not, the count walks everything the pruned pass just avoided, and
/// two passes beat one only in the wrong direction.
pub(crate) fn count_without_walking(query_json: &Option<Value>) -> bool {
    let Some(q) = query_json else { return true };
    let Some(obj) = q.as_object() else { return false };
    if obj.len() != 1 {
        return false;
    }
    match obj.keys().next().map(|k| k.as_str()) {
        Some("match_all") => true,
        // a term query on one field, with no per-term options that would make
        // it something else
        Some("term") => obj
            .values()
            .next()
            .and_then(|v| v.as_object())
            .map(|o| o.len() == 1 && o.values().next().map(|v| !v.is_object()).unwrap_or(false))
            .unwrap_or(false),
        _ => false,
    }
}

/// How many documents the query matches.
///
/// `Weight::count` reads it straight from the postings header where the query
/// allows -- a term query with no deletions knows its own document frequency --
/// and falls back to walking the matches where it does not.
pub(crate) fn count_matches(
    searcher: &Searcher,
    query: &dyn velocore::query::Query,
) -> velocore::Result<usize> {
    let weight = query.weight(velocore::query::EnableScoring::disabled_from_searcher(searcher))?;
    let mut total = 0usize;
    for reader in searcher.segment_readers() {
        total += weight.count(reader)? as usize;
    }
    Ok(total)
}

/// Turn what the shards collected into the answer a client reads.
///
/// The shards hand back intermediate results; combining them is VeloCore's
/// job, and everything after that is this engine's: the shapes OpenSearch
/// writes a bucket key in, the orders and partitions taken off the request
/// before it was parsed, and the `meta` a caller attached.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finalise_aggs(
    store: &Store,
    targets: &[String],
    acc: Option<IntermediateAggregationResults>,
    req: Option<Aggregations>,
    agg_json: &Option<Value>,
    bucket_orders: &[(String, String, bool)],
    partitions: &[(String, i64, i64, usize)],
    agg_meta: &[(String, Value)],
    weighted: bool,
) -> std::result::Result<Option<Value>, Response> {
    let out = match (acc, req) {
        (Some(acc), Some(req)) => match acc.into_final_result(req, Default::default()) {
            Ok(res) => serde_json::to_value(res).ok().map(|mut v| {
                recompute_extended_stats(&mut v);
                normalize_range_keys(&mut v);
                if let Some(req) = agg_json.as_ref() {
                    apply_bucket_formats(&mut v, req);
                    // a search may span indices, so a field's type is whatever
                    // the first index that names it says
                    let mut types: std::collections::HashMap<String, String> = targets
                        .iter()
                        .filter_map(|n| store.get(n))
                        .flat_map(|st| {
                            st.read()
                                .mapping
                                .types
                                .iter()
                                .map(|(k, t)| (k.clone(), t.clone()))
                                .collect::<Vec<_>>()
                        })
                        .collect();
                    // an index of the cluster this node holds no copy of: the
                    // mapping the manager published says what its fields are,
                    // and without it a key would come back unformatted
                    let published = crate::cluster::current_state();
                    for name in targets.iter().filter(|n| store.get(n).is_none()) {
                        if let Some(m) = published.indices.get(name) {
                            published_types(&m.mappings, "", &mut types);
                        }
                    }
                    date_histogram_keys(&mut v, req, &types);
                    format_terms_keys(&mut v, req, &types);
                    // one index may hold a field as whole numbers and another
                    // as fractions; the answer is one field, so the keys are
                    // written the wider way rather than two ways at once
                    let floating: std::collections::HashSet<String> = targets
                        .iter()
                        .filter_map(|n| store.get(n))
                        .flat_map(|st| {
                            let g = st.read();
                            g.observed_kinds
                                .iter()
                                .filter(|(_, k)| **k & crate::store::KIND_F64 != 0)
                                .map(|(f, _)| f.clone())
                                .collect::<Vec<_>>()
                        })
                        .collect();
                    widen_number_keys(&mut v, req, &floating);
                    cut_terms(&mut v, req);
                }
                if weighted {
                    apply_doc_counts(&mut v);
                    if let Some(req) = agg_json.as_ref() {
                        restore_key_orders(&mut v, req);
                    }
                }
                apply_bucket_orders(&mut v, bucket_orders);
                apply_partitions(&mut v, partitions);
                reattach_meta(&mut v, agg_meta);
                v
            }),
            Err(e) => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "aggregation_execution_exception",
                    e.to_string(),
                ));
            }
        },
        _ => None,
    };
    Ok(out)
}

/// Run the aggregations VeloCore could not parse, each as its own search.
pub(crate) fn run_peeled_aggs(
    store: &Store,
    targets: &[String],
    query_json: &Option<Value>,
    peeled: &[(String, Value)],
    weighted: bool,
) -> std::result::Result<Vec<(String, Value)>, Response> {
    let mut out: Vec<(String, Value)> = Vec::new();
    for (name, def) in peeled {
        // what the request attached to the aggregation travels with its answer
        let own_meta = def.get("meta").cloned();
        let mut v = run_peeled_agg(store, targets, query_json, name, def, weighted)?;
        // an aggregation answered on its own still carries the counts that
        // weight its buckets; a calendar histogram over documents that each
        // stand for several counted each once and showed the helpers
        if weighted {
            apply_doc_counts(&mut v);
        }
        if let Some(m) = own_meta {
            v["meta"] = m;
        }
        out.push((name.clone(), v));
    }
    Ok(out)
}

pub(crate) fn plan_aggs(
    store: &Store,
    targets: &[String],
    body: &Value,
) -> std::result::Result<AggPlan, Response> {
    let mut agg_json = body.get("aggs").or_else(|| body.get("aggregations")).cloned();
    if agg_json.as_ref().map(composite_under_a_parent).unwrap_or(false) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "[composite] aggregation cannot be used with a parent aggregation of type: [terms]",
        ));
    }
    // Parameter bounds do not depend on any mapping, so they are checked here
    // rather than per shard: a request that names no existing index has no
    // shards to walk, and a bad parameter would otherwise pass unread.
    if let Some(a) = agg_json.as_ref() {
        check_agg_bounds(a, "")?;
    }
    // buckets have to be weighted only where a document stands for several
    let weighted = targets.iter().filter_map(|n| store.get(n)).any(|st| st.read().has_doc_count);
    if weighted && let Some(a) = agg_json.as_mut() {
        inject_doc_count_helpers(a);
    }
    if let Some(a) = agg_json.as_mut() {
        // a filter aggregation can carry a terms lookup too
        resolve_terms_lookups(store, a)?;
    }
    // VeloCore has `filter` but not `filters`; peel those out and run them
    // ourselves as one filtered search per named bucket
    // sibling pipelines read the finished buckets, so they are held back and
    // computed once the rest of the aggregations have answered
    let mut pipeline_aggs: Vec<(String, Value)> = Vec::new();
    if let Some(Value::Object(o)) = agg_json.as_mut() {
        let names: Vec<String> =
            o.iter().filter(|(_, d)| is_pipeline_agg(d)).map(|(k, _)| k.clone()).collect();
        for n in names {
            if let Some(def) = o.remove(&n) {
                pipeline_aggs.push((n, def));
            }
        }
    }
    // a pipeline that sits *inside* a bucketing aggregation reads that
    // aggregation's own buckets, so it is taken out of the request and applied
    // to the answer once the buckets are there
    let mut bucket_pipelines: Vec<(Vec<String>, String, Value)> = Vec::new();
    if let Some(node) = agg_json.as_mut() {
        strip_bucket_pipelines(node, &mut Vec::new(), &mut bucket_pipelines);
    }
    // one of those written at the top level has no buckets to read
    if let Some((_, name, def)) = bucket_pipelines.iter().find(|(at, _, _)| at.is_empty()) {
        let kind = def
            .as_object()
            .and_then(|o| {
                o.keys()
                    .map(|k| k.to_string())
                    .find(|k| BUCKET_PIPELINES.contains(&k.as_str()) || k == "bucket_sort")
            })
            .unwrap_or_default();
        // the ones that read a series want a histogram over it; the rest only
        // want somewhere to sit
        let want = match kind.starts_with("bucket_") {
            true => "must be declared inside of another aggregation".to_string(),
            false => "must have a histogram, date_histogram or auto_date_histogram as parent \
                      but doesn't have a parent"
                .to_string(),
        };
        return Err(err(
            StatusCode::BAD_REQUEST,
            "action_request_validation_exception",
            format!("Validation Failed: 1: {kind} aggregation [{name}] {want};"),
        ));
    }
    let mut filters_aggs: Vec<(String, Value)> = Vec::new();
    if let Some(Value::Object(o)) = agg_json.as_mut() {
        let names: Vec<String> = o
            .iter()
            .filter(|(_, def)| {
                // anything under it that has to be run here drags the whole
                // aggregation out of VeloCore's hands with it
                peelable(def, store, targets)
                    || def.get("filters").is_some()
                    || def.get("missing").is_some()
                    || def.get("median_absolute_deviation").is_some()
                    // percentiles answer a different question from VeloCore's
                    // sketch, which is approximate where OpenSearch's is exact
                    // over the handful of values these aggregations see
                    || def.get("percentiles").is_some()
                    // `_index` is metadata, not a column: bucket it ourselves
                    || def.get("global").is_some()
                    || def
                        .get("terms")
                        .and_then(|t| t.get("field"))
                        .and_then(|f| f.as_str())
                        == Some("_index")
                    // an analysed field buckets what the analyser made of the
                    // text, which lives in the term dictionary rather than in
                    // a column of values
                    || def
                        .get("terms")
                        .and_then(|t| t.get("field"))
                        .and_then(|f| f.as_str())
                        .map(|f| analysed_text_field(store, targets, f))
                        .unwrap_or(false)
                    // VeloCore's own `filter` agg only speaks its query-string
                    // dialect, so run singular filters through our query builder
                    || def.get("filter").is_some()
                    || def.get("composite").is_some()
                    || def.get("multi_terms").is_some()
                    || def.get("rare_terms").is_some()
                    || def.get("nested").is_some()
                    || def.get("reverse_nested").is_some()
                    || def.get("sampler").is_some()
                    || def.get("children").is_some()
                    || def.get("parent").is_some()
                    || def.get("geo_bounds").is_some()
                    || def.get("geohash_grid").is_some()
                    || def.get("geotile_grid").is_some()
                    || def.get("geo_centroid").is_some()
                    || def.get("matrix_stats").is_some()
                    || def.get("diversified_sampler").is_some()
                    || def.get("geo_distance").is_some()
                    || def.get("percentile_ranks").is_some()
                    || def.get("significant_terms").is_some()
                    || def.get("significant_text").is_some()
                    || def.get("ip_range").is_some()
                    || def.get("date_range").is_some()
                    || def.get("adjacency_matrix").is_some()
                    || def.get("weighted_avg").is_some()
                    || def.get("auto_date_histogram").is_some()
                    || def.get("variable_width_histogram").is_some()
                    // calendar units are not fixed lengths, and a named zone is
                    // a history of offsets; a fixed step over the numbers the
                    // index holds is a plain histogram, which VeloCore runs
                    || def.get("date_histogram").map(walked_here).unwrap_or(false)
                    // a range field holds no single value to bucket a document
                    // by, so VeloCore's histogram sees nothing there at all
                    || def
                        .get("histogram")
                        .and_then(|h| h.get("field"))
                        .and_then(|f| f.as_str())
                        .map(|f| range_field(store, targets, f))
                        .unwrap_or(false)
                    // a field no document has, standing in for every document
                    || def
                        .get("terms")
                        .and_then(|t| t.get("field"))
                        .and_then(|f| f.as_str())
                        .map(|f| {
                            def.pointer("/terms/missing").is_some()
                                && unmapped_field(store, targets, f)
                        })
                        .unwrap_or(false)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for n in names {
            if let Some(def) = o.remove(&n) {
                filters_aggs.push((n, def));
            }
        }
        if o.is_empty() {
            agg_json = None;
        }
    }
    Ok(AggPlan {
        request: agg_json,
        peeled: filters_aggs,
        siblings: pipeline_aggs,
        inner: bucket_pipelines,
        weighted,
    })
}

pub(crate) fn combine(main: &Option<Value>, extra: Option<Value>) -> Value {
    match (main, extra) {
        (Some(m), Some(e)) => json!({"bool": {"must": [m.clone()], "filter": [e]}}),
        (Some(m), None) => m.clone(),
        (None, Some(e)) => e,
        (None, None) => json!({"match_all": {}}),
    }
}

/// Split sub-aggregations into the ones this engine computes itself and the
/// ones VeloCore can parse, so each set can take the path that suits it.
pub(crate) fn split_peelable(
    sub_aggs: &Option<Value>,
    store: &Store,
    targets: &[String],
) -> (Option<Value>, Option<Value>) {
    let Some(o) = sub_aggs.as_ref().and_then(|s| s.as_object()) else {
        return (None, sub_aggs.clone());
    };
    let (mine, theirs): (Vec<_>, Vec<_>) = o.iter().partition(|(_, d)| peelable(d, store, targets));
    let pack = |v: Vec<(&String, &Value)>| {
        if v.is_empty() {
            None
        } else {
            Some(Value::Object(v.into_iter().map(|(k, d)| (k.clone(), d.clone())).collect()))
        }
    };
    (pack(mine), pack(theirs))
}

/// Is this aggregation, or anything under it, one that has to be computed a
/// bucket at a time here?
pub(crate) fn peelable(def: &Value, store: &Store, targets: &[String]) -> bool {
    peelable_here(def, store, targets)
        || def
            .get("aggs")
            .or_else(|| def.get("aggregations"))
            .and_then(|s| s.as_object())
            .map(|o| o.values().any(|d| peelable(d, store, targets)))
            .unwrap_or(false)
}

/// Is this an aggregation VeloCore has no parser for, which has to be computed
/// a bucket at a time here instead?
pub(crate) fn peelable_here(def: &Value, store: &Store, targets: &[String]) -> bool {
    const OWN: &[&str] = &[
        "missing",
        "median_absolute_deviation",
        "filter",
        "filters",
        // approximate where OpenSearch is exact over the handful of values
        // these aggregations see
        "percentiles",
        "global",
        "weighted_avg",
        "variable_width_histogram",
        "auto_date_histogram",
        "date_range",
        "ip_range",
        "adjacency_matrix",
        "rare_terms",
        "multi_terms",
        "composite",
        "significant_terms",
        "significant_text",
        "top_hits",
        "nested",
        "reverse_nested",
        "geo_distance",
        "percentile_ranks",
        "children",
        "parent",
        "geo_bounds",
        "geo_centroid",
        "geohash_grid",
        "geotile_grid",
        "matrix_stats",
        "sampler",
        "diversified_sampler",
    ];
    OWN.iter().any(|k| def.get(k).is_some())
        // `_index` is not a column but a property of the whole index, so a
        // terms over it is counted here whatever it sits under; left to
        // VeloCore under another bucket, it found no column and no buckets
        || def.pointer("/terms/field").and_then(|f| f.as_str()) == Some("_index")
        // VeloCore's histogram and range read no `missing`, so the documents
        // without a value went uncounted
        || def.pointer("/histogram/missing").is_some()
        || def.pointer("/range/missing").is_some()
        // an analysed field buckets what the analyser made of the text, which
        // lives in the term dictionary rather than in a column of values
        || def
            .get("terms")
            .and_then(|t| t.get("field"))
            .and_then(|f| f.as_str())
            .map(|f| analysed_text_field(store, targets, f))
            .unwrap_or(false)
        || def.get("date_histogram").map(walked_here).unwrap_or(false)
        // a script makes the keys, which no engine reads from a field
        || def.pointer("/terms/script").is_some()
        || def.get("scripted_metric").is_some()
        // a metric whose value is worked out per document rather than read
        // out of a column
        || crate::search::aggs::metric::scripted_metric_kind(def).is_some()
}

/// A date histogram this engine has to walk itself, a bucket at a time: one
/// stepping by a calendar unit, one reported in a zone that is not simply UTC,
/// or one over a field whose numbers are not the milliseconds a key is in.
pub(crate) fn walked_here(spec: &Value) -> bool {
    if spec.get("calendar_interval").is_some() {
        return true;
    }
    if fixed_step_ms(spec).is_none() {
        return true;
    }
    // any zone but UTC has to be placed here: even one that is on UTC today
    // may not have been at the instant a bucket falls in
    match spec.get("time_zone").and_then(|v| v.as_str()).map(|z| z.trim()) {
        None | Some("") => false,
        Some(z) => !matches!(z, "Z" | "UTC" | "utc" | "+00:00" | "-00:00" | "+0000" | "-0000"),
    }
}

/// The step a date histogram takes, in milliseconds, when it is a fixed length.
pub(crate) fn fixed_step_ms(spec: &Value) -> Option<i64> {
    spec.get("fixed_interval")
        .or_else(|| spec.get("interval"))
        .and_then(|v| v.as_str())
        .and_then(parse_offset)
        .map(|d| d.whole_milliseconds() as i64)
        .filter(|ms| *ms > 0)
}

/// Turn a fixed-step date histogram into the histogram it is.
///
/// A date is milliseconds in the index, so a step of so many milliseconds over
/// that column is the same bucketing -- and VeloCore walks it in one pass
/// instead of this engine counting each bucket with its own query.
pub(crate) fn fixed_date_histograms(node: &mut Value, ctx: &Ctx) {
    let Some(map) = node.as_object_mut() else { return };
    for (_, def) in map.iter_mut() {
        let Some(d) = def.as_object_mut() else { continue };
        if let Some(sub) = d.get_mut("aggs") {
            fixed_date_histograms(sub, ctx);
        }
        let Some(spec) = d.get("date_histogram").cloned() else { continue };
        if walked_here(&spec) {
            continue;
        }
        let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
        // a date_nanos counts in nanoseconds, and a key is milliseconds
        if ctx.mapping.type_of(&field) != Some("date") {
            continue;
        }
        let Some(step) = fixed_step_ms(&spec) else { continue };
        let offset = spec
            .get("offset")
            .and_then(|v| v.as_str())
            .and_then(parse_offset)
            .map(|o| o.whole_milliseconds() as i64)
            .unwrap_or(0)
            .rem_euclid(step);
        let mut hist = json!({"field": field, "interval": step, "offset": offset});
        if let Some(min) = spec.get("min_doc_count") {
            hist["min_doc_count"] = min.clone();
        }
        for key in ["hard_bounds", "extended_bounds"] {
            let Some(b) = spec.get(key) else { continue };
            let edge = |name: &str| -> Option<i64> {
                crate::store::date_number(b.get(name)?, None, false)
            };
            if let (Some(min), Some(max)) = (edge("min"), edge("max")) {
                hist[key] = json!({"min": min, "max": max});
            }
        }
        d.remove("date_histogram");
        d.insert("histogram".into(), hist);
    }
}

/// Count the documents a query matches, and run its sub-aggregations --
/// including the ones VeloCore cannot parse, which are run here against the
/// same query rather than handed down.
pub(crate) fn count_with_sub_aggs(
    store: &Store,
    targets: &[String],
    query_json: &Value,
    sub_aggs: &Option<Value>,
    weighted: bool,
) -> std::result::Result<(u64, Option<Value>), Response> {
    let Some(subs) = sub_aggs.as_ref().and_then(|s| s.as_object()) else {
        return filtered_count(store, targets, query_json, sub_aggs);
    };
    let (mine, theirs): (Vec<_>, Vec<_>) =
        subs.iter().partition(|(_, d)| peelable(d, store, targets));
    if mine.is_empty() {
        return filtered_count(store, targets, query_json, sub_aggs);
    }
    let rest: Option<Value> = if theirs.is_empty() {
        None
    } else {
        Some(Value::Object(theirs.into_iter().map(|(k, v)| (k.clone(), v.clone())).collect()))
    };
    let (count, mut out) = filtered_count(store, targets, query_json, &rest)?;
    let base = Some(query_json.clone());
    let mut merged = out.take().and_then(|v| v.as_object().cloned()).unwrap_or_default();
    for (n, d) in mine {
        merged.insert(n.clone(), run_peeled_agg(store, targets, &base, n, d, weighted)?);
    }
    Ok((count, Some(Value::Object(merged))))
}

/// What was taken out from under one of VeloCore's bucket aggregations: the
/// sub-aggregations run here, a bucket at a time, and the same for the
/// bucket aggregations still inside it.
struct HeldBack {
    peeled: Vec<(String, Value)>,
    inner: Vec<(String, HeldBack)>,
}

/// Take the sub-aggregations VeloCore cannot run out from under the bucket
/// aggregations it can, at any depth, and say what was taken from where.
fn hold_back_peeled(
    node: &mut Value,
    store: &Store,
    targets: &[String],
) -> Vec<(String, HeldBack)> {
    let Some(map) = node.as_object_mut() else { return Vec::new() };
    let mut out = Vec::new();
    for (name, def) in map.iter_mut() {
        let Some(d) = def.as_object_mut() else { continue };
        let key = match (d.contains_key("aggs"), d.contains_key("aggregations")) {
            (true, _) => "aggs",
            (false, true) => "aggregations",
            _ => continue,
        };
        let Some(Value::Object(subs)) = d.get_mut(key) else { continue };
        let names: Vec<String> = subs
            .iter()
            .filter(|(_, s)| peelable(s, store, targets))
            .map(|(k, _)| k.clone())
            .collect();
        let peeled: Vec<(String, Value)> =
            names.iter().filter_map(|n| subs.shift_remove(n).map(|v| (n.clone(), v))).collect();
        let empty = subs.is_empty();
        let inner = match d.get_mut(key) {
            Some(rest) => hold_back_peeled(rest, store, targets),
            None => Vec::new(),
        };
        if empty {
            d.remove(key);
        }
        if !peeled.is_empty() || !inner.is_empty() {
            out.push((name.clone(), HeldBack { peeled, inner }));
        }
    }
    out
}

/// The query that picks out the documents of one bucket VeloCore made.
///
/// A key is enough to name a `terms` bucket, and the edges a `histogram` or a
/// `range` one; a value the request said to stand in for a missing one also
/// takes in the documents that have none.
pub(crate) fn bucket_filter(
    store: &Store,
    targets: &[String],
    def: &Value,
    bucket: &Value,
) -> Option<Value> {
    let (kind, spec) = def.as_object()?.iter().find(|(k, _)| {
        matches!(String::as_str(k), "terms" | "histogram" | "date_histogram" | "range")
    })?;
    let field = spec.get("field")?.as_str()?.to_string();
    let ty = targets
        .iter()
        .filter_map(|n| store.get(n))
        .find_map(|st| st.read().mapping.type_of(&field).map(|t| t.to_string()));
    let span = |gte: Option<f64>, lt: Option<f64>| {
        let mut clause = serde_json::Map::new();
        if let Some(v) = gte {
            clause.insert("gte".into(), json!(v));
        }
        if let Some(v) = lt {
            clause.insert("lt".into(), json!(v));
        }
        if matches!(ty.as_deref(), Some("date" | "date_nanos")) {
            clause.insert("format".into(), json!("epoch_millis"));
        }
        json!({"range": {field.clone(): Value::Object(clause)}})
    };
    let (filter, stand_in) = match kind.as_str() {
        "terms" => {
            let key = bucket.get("key")?;
            let filter = match ty.as_deref() {
                Some("date" | "date_nanos") => {
                    let ms = key.as_f64()?;
                    json!({"range": {field.clone(): {"gte": ms, "lte": ms, "format": "epoch_millis"}}})
                }
                Some("boolean") => json!({"term": {field.clone(): key.as_u64()? != 0}}),
                _ => json!({"term": {field.clone(): key}}),
            };
            let stands_in = spec.get("missing").map(|m| {
                let shown = bucket.get("key_as_string").cloned().unwrap_or_else(|| key.clone());
                m == key || *m == shown || m.as_f64().is_some() && m.as_f64() == key.as_f64()
            });
            (filter, stands_in.unwrap_or(false))
        }
        "histogram" | "date_histogram" => {
            let key = bucket.get("key")?.as_f64()?;
            let step = match kind.as_str() {
                "histogram" => spec.get("interval")?.as_f64()?,
                _ => fixed_step_ms(spec)? as f64,
            };
            let filter = span(Some(key), Some(key + step));
            let stands_in =
                spec.get("missing").and_then(|m| m.as_f64()).map(|m| m >= key && m < key + step);
            (filter, stands_in.unwrap_or(false))
        }
        _ => {
            let from = bucket.get("from").and_then(|v| v.as_f64());
            let to = bucket.get("to").and_then(|v| v.as_f64());
            let filter = span(from, to);
            let stands_in = spec
                .get("missing")
                .and_then(|m| m.as_f64())
                .map(|m| from.map(|f| m >= f).unwrap_or(true) && to.map(|t| m < t).unwrap_or(true));
            (filter, stands_in.unwrap_or(false))
        }
    };
    Some(match stand_in {
        true => json!({"bool": {"should": [
            filter,
            {"bool": {"must_not": [{"exists": {"field": field}}]}},
        ], "minimum_should_match": 1}}),
        false => filter,
    })
}

/// Run what `hold_back_peeled` took out, in each bucket it was taken from.
fn fill_held_back(
    store: &Store,
    targets: &[String],
    base: &Value,
    answer: &mut Value,
    request: &Value,
    held: &[(String, HeldBack)],
) -> std::result::Result<(), Response> {
    for (name, back) in held {
        let Some(def) = request.get(name) else { continue };
        let subs = def.get("aggs").or_else(|| def.get("aggregations"));
        let Some(node) = answer.get_mut(name) else { continue };
        let fill = |bucket: &mut Value| -> std::result::Result<(), Response> {
            let Some(filter) = bucket_filter(store, targets, def, bucket) else { return Ok(()) };
            let narrowed = json!({"bool": {"filter": [base.clone(), filter]}});
            for (n, d) in &back.peeled {
                bucket[n.clone()] =
                    run_peeled_agg(store, targets, &Some(narrowed.clone()), n, d, false)?;
            }
            if let Some(subs) = subs {
                fill_held_back(store, targets, &narrowed, bucket, subs, &back.inner)?;
            }
            Ok(())
        };
        match node.get_mut("buckets") {
            Some(Value::Array(list)) => {
                for b in list.iter_mut() {
                    fill(b)?;
                }
            }
            Some(Value::Object(keyed)) => {
                for b in keyed.values_mut() {
                    fill(b)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Count what a query matches and run the aggregations under it.
///
/// VeloCore runs them, except for what sits under one of its bucket
/// aggregations and is not its to run -- a `top_hits`, a `rare_terms`, any of
/// the aggregations this engine walks itself. Those were handed to VeloCore
/// with the rest, which refused a `top_hits` without a `sort` and answered one
/// with a sort with hits that held no document, so under a `rare_terms` or a
/// `composite` a `top_hits` did not work at all. They are taken out first and
/// run afterwards in each bucket, narrowed to that bucket's documents.
pub(crate) fn filtered_count(
    store: &Store,
    targets: &[String],
    query_json: &Value,
    sub_aggs: &Option<Value>,
) -> std::result::Result<(u64, Option<Value>), Response> {
    let Some(asked) = sub_aggs.as_ref() else {
        return velocore_count(store, targets, query_json, sub_aggs);
    };
    let mut plain = asked.clone();
    let held = hold_back_peeled(&mut plain, store, targets);
    if held.is_empty() {
        return velocore_count(store, targets, query_json, sub_aggs);
    }
    let (count, mut out) = velocore_count(store, targets, query_json, &Some(plain))?;
    if let Some(answer) = out.as_mut() {
        fill_held_back(store, targets, query_json, answer, asked, &held)?;
    }
    Ok((count, out))
}

fn velocore_count(
    store: &Store,
    targets: &[String],
    query_json: &Value,
    sub_aggs: &Option<Value>,
) -> std::result::Result<(u64, Option<Value>), Response> {
    let mut total = 0u64;
    let mut acc: Option<IntermediateAggregationResults> = None;
    let mut req: Option<Aggregations> = None;
    for name in targets {
        let Some(st) = store.get(name) else { continue };
        let g = st.read();
        let ctx = Ctx {
            fields: &g.fields,
            mapping: &g.mapping,
            analysis: &g.analysis,
            index: &g.index,
            max_terms_count: g.max_terms_count(),
            max_regex_length: g.max_regex_length(),
            allow_expensive: crate::search::expensive_allowed(store),
            observed_kinds: &g.observed_kinds,
            kinds_complete: g.kinds_complete,
            stats: &g.stats,
            vectors: &g.vectors,
        };
        // the caller's own view of this index: the document filter their
        // role carries, and the fields they may not aggregate over
        let (narrowed, narrowed_aggs) =
            crate::security::view::narrowed_for(store, name, &g, query_json, sub_aggs);
        let sub_aggs = &narrowed_aggs;
        let q = crate::query::build(&ctx, &narrowed)
            .map_err(|e| err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string()))?;
        let searcher = g.reader.searcher();
        total += searcher
            .search(&q, &Count)
            .map_err(|e| crate::search::search_error_response(&e.to_string(), name))?
            as u64;

        if let Some(sa) = sub_aggs {
            let mut rewritten = sa.clone();
            let mut ignored = Vec::new();
            normalize_aggs(&mut rewritten, &mut ignored, false);
            // the same preparation a top-level aggregation gets: a date is
            // written many ways and a fixed step is one of them, and a
            // sub-aggregation is no different for being one
            normalize_agg_dates(&mut rewritten);
            lower_nested_filters(&mut rewritten, &ctx);
            strip_untranslatable_term_filters(&mut rewritten, &ctx);
            fixed_date_histograms(&mut rewritten, &ctx);
            rewrite_agg_fields(&mut rewritten, &ctx);
            let parsed: Aggregations = serde_json::from_value(rewritten)
                .map_err(|e| err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string()))?;
            let ctxp = AggContextParams::new(Default::default(), g.index.tokenizers().clone());
            let res = searcher
                .search(&q, &DistributedAggregationCollector::from_aggs(parsed.clone(), ctxp))
                .map_err(|e| {
                    err(StatusCode::BAD_REQUEST, "aggregation_execution_exception", e.to_string())
                })?;
            match acc.as_mut() {
                Some(a) => {
                    let _ = a.merge_fruits(res);
                }
                None => acc = Some(res),
            }
            req = Some(parsed);
        }
    }
    // a sub-aggregation's answer is written the way a top-level one is: the
    // key shapes, the formats and the names a date metric carries
    let mut sub = finalise_aggs(store, targets, acc, req, sub_aggs, &[], &[], &[], false)?;
    // and the same finishing touches the answer gets at the top: keys in
    // milliseconds, the range keys the request asked for, the names a date
    // metric carries
    if let (Some(a), Some(reqj)) = (sub.as_mut(), sub_aggs.as_ref()) {
        millis_in_keys(a);
        keep_asked_ranges(reqj, a);
        whole_metric_values(a, reqj);
        name_date_metrics(store, targets, reqj, a);
    }
    Ok((total, sub))
}

/// The field types a published mapping names, as the aggregations read them.
pub(crate) fn published_types(
    mappings: &Value,
    prefix: &str,
    out: &mut std::collections::HashMap<String, String>,
) {
    let Some(props) = mappings.get("properties").and_then(|p| p.as_object()) else { return };
    for (name, def) in props {
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        if let Some(t) = def.get("type").and_then(|t| t.as_str()) {
            out.entry(path.clone()).or_insert_with(|| t.to_string());
        }
        if def.get("properties").is_some() {
            published_types(def, &path, out);
        }
        if let Some(fields) = def.get("fields").and_then(|f| f.as_object()) {
            for (sub, sdef) in fields {
                if let Some(t) = sdef.get("type").and_then(|t| t.as_str()) {
                    out.entry(format!("{path}.{sub}")).or_insert_with(|| t.to_string());
                }
            }
        }
    }
}
