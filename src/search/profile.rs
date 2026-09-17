//! What a search cost, in the shape OpenSearch reports it.
//!
//! OpenSearch profiles each shard: the query as the tree of Lucene queries it
//! was rewritten into, each timed; the collectors; each aggregation with its
//! own phases; and the fetch. This node holds every shard of an index in one
//! reader, so each phase is timed once, over the whole index, and every
//! figure here is a measurement of the work named. Where the reference reports
//! a shard this node did not search on its own, the index's measurements are
//! shared between its shards by the documents each shard contributed: the
//! counts are each shard's own, and the times are the index's in proportion
//! to them -- see `split_by_shard`.

use super::*;
use std::time::Instant;

/// One timed part of a profiled phase, segment by segment.
#[derive(Default, Clone)]
struct Part {
    /// time spent in each segment, in nanoseconds
    nanos: Vec<u64>,
    /// how many times the part ran in each segment
    counts: Vec<u64>,
}

impl Part {
    fn add(&mut self, nanos: u64, count: u64) {
        self.nanos.push(nanos);
        self.counts.push(count);
    }

    fn total(&self) -> u64 {
        self.nanos.iter().sum()
    }

    fn count(&self) -> u64 {
        self.counts.iter().sum()
    }
}

/// Write a part into a breakdown: its time and count, and -- for the parts
/// timed per segment, as concurrent segment search times them per slice --
/// the least, most and mean across the segments.
fn write_part(
    out: &mut std::collections::BTreeMap<String, Value>,
    name: &str,
    part: &Part,
    sliced: bool,
    sliced_counts: bool,
) {
    out.insert(name.to_string(), json!(part.total()));
    out.insert(format!("{name}_count"), json!(part.count()));
    if !sliced {
        return;
    }
    let spread = |values: &[u64]| -> (u64, u64, u64) {
        match values.is_empty() {
            true => (0, 0, 0),
            false => (
                *values.iter().min().unwrap_or(&0),
                *values.iter().max().unwrap_or(&0),
                values.iter().sum::<u64>() / values.len() as u64,
            ),
        }
    };
    let (min, max, avg) = spread(&part.nanos);
    out.insert(format!("min_{name}"), json!(min));
    out.insert(format!("max_{name}"), json!(max));
    out.insert(format!("avg_{name}"), json!(avg));
    if sliced_counts {
        let (min, max, avg) = spread(&part.counts);
        out.insert(format!("min_{name}_count"), json!(min));
        out.insert(format!("max_{name}_count"), json!(max));
        out.insert(format!("avg_{name}_count"), json!(avg));
    }
}

/// The least, most and mean time of the segments, as a profile's slice
/// figures.
fn slice_figures(entry: &mut Value, per_segment: &[u64]) {
    let (min, max, avg) = match per_segment.is_empty() {
        true => (0, 0, 0),
        false => (
            *per_segment.iter().min().unwrap_or(&0),
            *per_segment.iter().max().unwrap_or(&0),
            per_segment.iter().sum::<u64>() / per_segment.len() as u64,
        ),
    };
    entry["max_slice_time_in_nanos"] = json!(max);
    entry["min_slice_time_in_nanos"] = json!(min);
    entry["avg_slice_time_in_nanos"] = json!(avg);
}

/// What running one query over a searcher cost, part by part.
#[derive(Default)]
struct QueryCost {
    create_weight: Part,
    build_scorer: Part,
    next_doc: Part,
    score: Part,
    /// the whole of each segment's work
    segments: Vec<u64>,
}

/// Run a query the way a collector runs it, timing each part: the weight,
/// a scorer per segment, stepping through the documents it matches and, when
/// the search scores, scoring them.
fn time_query(searcher: &Searcher, q: &dyn velocore::query::Query, scoring: bool) -> QueryCost {
    use velocore::DocSet;
    let mut cost = QueryCost::default();
    let t = Instant::now();
    let enable = match scoring {
        true => velocore::query::EnableScoring::enabled_from_searcher(searcher),
        false => velocore::query::EnableScoring::disabled_from_searcher(searcher),
    };
    let Ok(weight) = q.weight(enable) else { return cost };
    cost.create_weight.add(t.elapsed().as_nanos() as u64, 1);
    for reader in searcher.segment_readers() {
        let segment = Instant::now();
        let t = Instant::now();
        let Ok(mut scorer) = weight.scorer(reader, 1.0) else { continue };
        cost.build_scorer.add(t.elapsed().as_nanos() as u64, 1);
        let alive = reader.alive_bitset();
        let (mut stepped, mut steps, mut scored, mut scores) = (0u64, 0u64, 0u64, 0u64);
        let mut t = Instant::now();
        loop {
            let doc = scorer.doc();
            if doc == velocore::TERMINATED {
                break;
            }
            if scoring && alive.map(|a| a.is_alive(doc)).unwrap_or(true) {
                stepped += t.elapsed().as_nanos() as u64;
                let s = Instant::now();
                let _ = scorer.score();
                scored += s.elapsed().as_nanos() as u64;
                scores += 1;
                t = Instant::now();
            }
            steps += 1;
            scorer.advance();
        }
        stepped += t.elapsed().as_nanos() as u64;
        cost.next_doc.add(stepped, steps);
        cost.score.add(scored, scores);
        cost.segments.push(segment.elapsed().as_nanos() as u64);
    }
    cost
}

/// The breakdown of a profiled query, with every key OpenSearch writes. The
/// parts this engine's scorers do not have -- advancing to a target, the
/// competitive-score hooks of block-max pruning, matching a two-phase
/// iterator -- ran no times and took no time. A search run concurrently, as
/// OpenSearch runs one with aggregations, adds each part's spread across the
/// slices.
fn query_breakdown(cost: &QueryCost, concurrent: bool) -> Value {
    let mut out = std::collections::BTreeMap::new();
    let none = Part::default();
    for (name, part) in [
        ("advance", &none),
        ("build_scorer", &cost.build_scorer),
        ("compute_max_score", &none),
        ("match", &none),
        ("next_doc", &cost.next_doc),
        ("score", &cost.score),
        ("set_min_competitive_score", &none),
        ("shallow_advance", &none),
    ] {
        write_part(&mut out, name, part, concurrent, concurrent);
    }
    write_part(&mut out, "create_weight", &cost.create_weight, false, false);
    Value::Object(out.into_iter().collect())
}

/// Whether a field holds numbers, as a query over it sees them.
fn numeric_mapping(field: &str, ctx: &Ctx) -> bool {
    matches!(
        ctx.mapping.type_of(field),
        Some("long" | "integer" | "short" | "byte" | "double" | "float" | "half_float")
            | Some("scaled_float" | "unsigned_long" | "date" | "date_nanos")
    )
}

/// Whether a query is a single term of a field that keeps no frequencies,
/// which OpenSearch wraps in a constant score wherever it is scored.
fn constant_term(json: &Value, ctx: &Ctx) -> bool {
    let Some((kind, spec)) = json.as_object().and_then(|o| o.iter().next()) else {
        return false;
    };
    if !matches!(kind.as_str(), "term" | "match") {
        return false;
    }
    let Some(field) = spec.as_object().and_then(|o| o.keys().next()) else { return false };
    matches!(ctx.mapping.type_of(field), Some("keyword" | "boolean" | "ip"))
}

/// What the profile prints for a match-all at the top of a search, which
/// OpenSearch runs through its approximation.
const APPROXIMATE_ALL: &str =
    "ApproximateScoreQuery(originalQuery=*:*, approximationQuery=Approximate(*:*))";

/// How a clause prints inside the query that holds it: scored where it is
/// scored, and in parentheses when it holds clauses of its own.
fn printed(json: &Value, ctx: &Ctx, scored: bool) -> String {
    let (kind, desc, _) = describe_query(json, ctx);
    match kind.as_str() {
        _ if scored && constant_term(json, ctx) => format!("ConstantScore({desc})"),
        "BooleanQuery" => format!("({desc})"),
        _ => desc,
    }
}

/// The Lucene query an OpenSearch query becomes, as the profile names it:
/// its class, how it prints, and the clauses under it, each with whether it
/// is scored there.
fn describe_query(json: &Value, ctx: &Ctx) -> (String, String, Vec<(Value, bool)>) {
    let Some((kind, spec)) = json.as_object().and_then(|o| o.iter().next()) else {
        return ("MatchAllDocsQuery".into(), "*:*".into(), Vec::new());
    };
    let field_value = |spec: &Value| -> Option<(String, Value)> {
        let (field, v) = spec.as_object()?.iter().next()?;
        let v = match v {
            Value::Object(o) => o.get("value").or_else(|| o.get("query")).cloned()?,
            other => other.clone(),
        };
        Some((field.clone(), v))
    };
    let text = |v: &Value| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let points = |field: &str, low: String, high: String| {
        let range = format!("{field}:[{low} TO {high}]");
        format!("IndexOrDocValuesQuery(indexQuery={range}, dvQuery={range})")
    };
    match kind.as_str() {
        "match_all" => ("MatchAllDocsQuery".into(), "*:*".into(), Vec::new()),
        "match_none" => ("MatchNoDocsQuery".into(), "MatchNoDocsQuery(\"\")".into(), Vec::new()),
        "term" | "match" | "match_phrase" => {
            let Some((field, v)) = field_value(spec) else {
                return ("TermQuery".into(), String::new(), Vec::new());
            };
            if numeric_mapping(&field, ctx) {
                let v = text(&v);
                return ("IndexOrDocValuesQuery".into(), points(&field, v.clone(), v), Vec::new());
            }
            let words: Vec<String> = match (kind.as_str(), ctx.mapping.type_of(&field)) {
                ("term", _) | (_, Some("keyword")) => vec![text(&v)],
                _ => text(&v).split_whitespace().map(|w| w.to_lowercase()).collect(),
            };
            match words.len() {
                0 | 1 => (
                    "TermQuery".into(),
                    format!("{field}:{}", words.first().cloned().unwrap_or_default()),
                    Vec::new(),
                ),
                _ if kind == "match_phrase" => {
                    ("PhraseQuery".into(), format!("{field}:\"{}\"", words.join(" ")), Vec::new())
                }
                _ => {
                    let clauses: Vec<(Value, bool)> =
                        words.iter().map(|w| (json!({"term": {field.clone(): w}}), true)).collect();
                    let desc = words.iter().map(|w| format!("{field}:{w}")).collect::<Vec<_>>();
                    ("BooleanQuery".into(), desc.join(" "), clauses)
                }
            }
        }
        "terms" => {
            let Some((field, v)) = spec.as_object().and_then(|o| o.iter().next()) else {
                return ("TermInSetQuery".into(), String::new(), Vec::new());
            };
            let values: Vec<String> =
                v.as_array().map(|a| a.iter().map(text).collect()).unwrap_or_default();
            ("TermInSetQuery".into(), format!("{field}:({})", values.join(" ")), Vec::new())
        }
        "ids" => {
            let values: Vec<String> = spec
                .get("values")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().map(text).collect())
                .unwrap_or_default();
            ("TermInSetQuery".into(), format!("_id:({})", values.join(" ")), Vec::new())
        }
        "range" => {
            let Some((field, bounds)) = spec.as_object().and_then(|o| o.iter().next()) else {
                return ("PointRangeQuery".into(), String::new(), Vec::new());
            };
            let bound = |keys: [&str; 2]| {
                keys.iter().find_map(|k| bounds.get(*k).filter(|v| !v.is_null())).map(text)
            };
            let low = bound(["gte", "from"]).or_else(|| bound(["gt", "gt"]));
            let high = bound(["lte", "to"]).or_else(|| bound(["lt", "lt"]));
            if numeric_mapping(field, ctx) {
                let (min, max) = match ctx.mapping.type_of(field) {
                    Some("integer") => ("-2147483648", "2147483647"),
                    Some("short") => ("-32768", "32767"),
                    Some("byte") => ("-128", "127"),
                    Some("double" | "float" | "half_float" | "scaled_float") => {
                        ("-Infinity", "Infinity")
                    }
                    _ => ("-9223372036854775808", "9223372036854775807"),
                };
                let low = low.unwrap_or_else(|| min.to_string());
                let high = high.unwrap_or_else(|| max.to_string());
                return ("IndexOrDocValuesQuery".into(), points(field, low, high), Vec::new());
            }
            let open = if bounds.get("gt").is_some() { "{" } else { "[" };
            let close = if bounds.get("lt").is_some() { "}" } else { "]" };
            (
                "TermRangeQuery".into(),
                format!(
                    "{field}:{open}{} TO {}{close}",
                    low.unwrap_or_else(|| "*".into()),
                    high.unwrap_or_else(|| "*".into())
                ),
                Vec::new(),
            )
        }
        "exists" => {
            let field = spec.get("field").map(text).unwrap_or_default();
            ("FieldExistsQuery".into(), format!("FieldExistsQuery [field={field}]"), Vec::new())
        }
        "prefix" => {
            let (field, v) = field_value(spec).unwrap_or_default();
            ("PrefixQuery".into(), format!("{field}:{}*", text(&v)), Vec::new())
        }
        "wildcard" => {
            let (field, v) = field_value(spec).unwrap_or_default();
            ("WildcardQuery".into(), format!("{field}:{}", text(&v)), Vec::new())
        }
        "constant_score" => {
            let inner = spec.get("filter").cloned().unwrap_or_else(|| json!({"match_all": {}}));
            let desc = printed(&inner, ctx, false);
            ("ConstantScoreQuery".into(), format!("ConstantScore({desc})"), vec![(inner, false)])
        }
        "bool" => {
            let mut clauses: Vec<(Value, bool)> = Vec::new();
            let mut shown: Vec<String> = Vec::new();
            // the order OpenSearch adds a bool query's clauses in; a filter
            // and a must_not are not scored, so they print bare
            for (occur, mark, scored) in [
                ("must", "+", true),
                ("must_not", "-", false),
                ("should", "", true),
                ("filter", "#", false),
            ] {
                let listed = match spec.get(occur) {
                    Some(Value::Array(a)) => a.clone(),
                    Some(one @ Value::Object(_)) => vec![one.clone()],
                    _ => Vec::new(),
                };
                for clause in listed {
                    shown.push(format!("{mark}{}", printed(&clause, ctx, scored)));
                    clauses.push((clause, scored));
                }
            }
            if clauses.is_empty() {
                return ("MatchAllDocsQuery".into(), "*:*".into(), Vec::new());
            }
            ("BooleanQuery".into(), shown.join(" "), clauses)
        }
        other => (
            format!("{}Query", capitalise_words(other)),
            serde_json::to_string(spec).unwrap_or_default(),
            Vec::new(),
        ),
    }
}

/// One profiled query node, from what running it cost.
fn timed_node(
    kind: &str,
    description: String,
    cost: &QueryCost,
    concurrent: bool,
    children: Vec<Value>,
) -> Value {
    let total = cost.create_weight.total()
        + cost.build_scorer.total()
        + cost.next_doc.total()
        + cost.score.total();
    let mut node = json!({
        "type": kind,
        "description": description,
        "time_in_nanos": total,
    });
    if concurrent {
        slice_figures(&mut node, &cost.segments);
    }
    node["breakdown"] = query_breakdown(cost, concurrent);
    if !children.is_empty() {
        node["children"] = json!(children);
    }
    node
}

/// The cost of a query the request wrote, built and run on its own.
fn cost_of(searcher: &Searcher, ctx: &Ctx, json: &Value, scoring: bool) -> QueryCost {
    match crate::query::build(ctx, json) {
        Ok(q) => time_query(searcher, q.as_ref(), scoring),
        Err(_) => QueryCost::default(),
    }
}

/// One node of the profiled query tree, and the nodes under it. `scored` says
/// whether the query is scored where it stands, and `top` whether it is the
/// whole of the search's query.
fn query_node(
    searcher: &Searcher,
    ctx: &Ctx,
    json: &Value,
    scoring: bool,
    concurrent: bool,
    (scored, top): (bool, bool),
) -> Value {
    let is_all = json.get("match_all").is_some();
    if top && is_all {
        let cost = cost_of(searcher, ctx, json, scoring);
        let inner =
            timed_node("ApproximateScoreQuery", APPROXIMATE_ALL.into(), &cost, concurrent, vec![]);
        let outer = format!("ConstantScore({APPROXIMATE_ALL})");
        return timed_node("ConstantScoreQuery", outer, &cost, concurrent, vec![inner]);
    }
    if scored && constant_term(json, ctx) {
        let inner = query_node(searcher, ctx, json, scoring, concurrent, (false, false));
        let cost = cost_of(searcher, ctx, json, scoring);
        let outer = format!("ConstantScore({})", printed(json, ctx, false));
        return timed_node("ConstantScoreQuery", outer, &cost, concurrent, vec![inner]);
    }
    let (kind, description, clauses) = describe_query(json, ctx);
    let cost = cost_of(searcher, ctx, json, scoring);
    let children: Vec<Value> = clauses
        .iter()
        .map(|(c, scored)| query_node(searcher, ctx, c, scoring, concurrent, (*scored, false)))
        .collect();
    timed_node(&kind, description, &cost, concurrent, children)
}

/// A shard's `searches` section: the query tree, how long rewriting it took,
/// and the collectors the search ran.
///
/// OpenSearch runs a search with aggregations concurrently, segment slices at
/// a time, and profiles it as such: collector managers, and each figure's
/// spread across the slices. A search without them runs one collector over
/// the segments in turn, and its profile says only what each part cost.
#[allow(clippy::too_many_arguments)]
pub(crate) fn search_profile(
    searcher: &Searcher,
    ctx: &Ctx,
    query_json: &Option<Value>,
    (scoring, sorted): (bool, bool),
    search_nanos: u64,
    agg_names: &[String],
    agg_nanos: u64,
    size: usize,
) -> Value {
    let json = query_json.clone().unwrap_or_else(|| json!({"match_all": {}}));
    let concurrent = !agg_names.is_empty();
    // rewriting is building the engine's query from the request's
    let t = Instant::now();
    let _ = crate::query::build(ctx, &json);
    let rewrite = t.elapsed().as_nanos() as u64;
    let tree = query_node(searcher, ctx, &json, scoring, concurrent, (scoring, true));
    let collector = |name: &str, reason: &str, nanos: u64| match concurrent {
        true => json!({
            "name": name, "reason": reason, "time_in_nanos": nanos,
            "reduce_time_in_nanos": 0,
            "max_slice_time_in_nanos": nanos, "min_slice_time_in_nanos": nanos,
            "avg_slice_time_in_nanos": nanos, "slice_count": 1,
        }),
        false => json!({"name": name, "reason": reason, "time_in_nanos": nanos}),
    };
    let collectors = match concurrent {
        false => {
            let name = match (size, sorted) {
                (0, _) => "EarlyTerminatingCollector",
                (_, true) => "SimpleFieldCollector",
                _ => "TopScoreDocCollector",
            };
            let reason = if size == 0 { "search_count" } else { "search_top_hits" };
            json!([collector(name, reason, search_nanos)])
        }
        true => {
            let hits = match size {
                0 => collector("TotalHitCountCollectorManager", "search_count", search_nanos),
                _ => collector("SimpleTopDocsCollectorManager", "search_top_hits", search_nanos),
            };
            let named = match agg_names.len() {
                1 => format!("[{}]", agg_names[0]),
                _ => format!("[[{}]]", agg_names.join(", ")),
            };
            let aggs = collector(
                &format!("NonGlobalAggCollectorManager: {named}"),
                "aggregation",
                agg_nanos,
            );
            let mut top =
                collector("QueryCollectorManager", "search_multi", search_nanos + agg_nanos);
            top["children"] = json!([hits, aggs]);
            json!([top])
        }
    };
    json!({"query": [tree], "rewrite_time": rewrite, "collector": collectors})
}

/// How many of the documents a query matches each shard of the index holds,
/// placed by the same fold the index's writes use. A profile is shared out
/// between the shards by these.
pub(crate) fn matched_by_shard(
    searcher: &Searcher,
    g: &IdxState,
    q: &dyn velocore::query::Query,
) -> Vec<u64> {
    // enough documents to say how the matches fall, without reading the id
    // of every one of a very large match
    const SAMPLE: usize = 200_000;
    let shards = g.shard_count().max(1) as usize;
    let mut out = vec![0u64; shards];
    let Ok(found) = searcher.search(q, &velocore::collector::DocSetCollector) else {
        return out;
    };
    let mut id = String::new();
    for addr in found.into_iter().take(SAMPLE) {
        let reader = searcher.segment_reader(addr.segment_ord);
        let Ok(Some(column)) = reader.fast_fields().str("_id") else { continue };
        let Some(ord) = column.term_ords(addr.doc_id).next() else { continue };
        id.clear();
        if column.ord_to_str(ord, &mut id).is_ok() {
            let shard = g.shard_of_doc(&id) as usize;
            if shard < shards {
                out[shard] += 1;
            }
        }
    }
    out
}

/// What running aggregations cost, phase by phase.
#[derive(Default)]
struct AggCost {
    initialize: Part,
    build_leaf_collector: Part,
    collect: Part,
    post_collection: Part,
    build_aggregation: Part,
    segments: Vec<u64>,
}

impl AggCost {
    fn total(&self) -> u64 {
        self.initialize.total()
            + self.build_leaf_collector.total()
            + self.collect.total()
            + self.post_collection.total()
            + self.build_aggregation.total()
    }

    fn breakdown(&self) -> Value {
        let mut out = std::collections::BTreeMap::new();
        write_part(&mut out, "initialize", &self.initialize, true, false);
        write_part(&mut out, "build_leaf_collector", &self.build_leaf_collector, true, true);
        write_part(&mut out, "collect", &self.collect, true, true);
        write_part(&mut out, "post_collection", &self.post_collection, true, false);
        write_part(&mut out, "build_aggregation", &self.build_aggregation, true, false);
        // the reduce runs once the shards' answers are merged, which is not
        // a phase of any one shard
        write_part(&mut out, "reduce", &Part { nanos: vec![0], counts: vec![0] }, true, false);
        Value::Object(out.into_iter().collect())
    }
}

/// Run aggregations over a query with the phase boundaries laid bare.
///
/// `searcher.search` folds the whole run into one call, so the phases are
/// driven here instead: building the collector, a leaf collector per segment,
/// the scan, the harvest, and the merge.
fn timed_aggs(
    searcher: &Searcher,
    weight: &dyn velocore::query::Weight,
    aggs: Aggregations,
    ctx: &Ctx,
) -> (velocore::Result<IntermediateAggregationResults>, AggCost) {
    use velocore::collector::{Collector, SegmentCollector};
    let mut cost = AggCost::default();
    let t = Instant::now();
    let ctxp = AggContextParams::new(Default::default(), ctx.index.tokenizers().clone());
    let collector = DistributedAggregationCollector::from_aggs(aggs, ctxp);
    cost.initialize.add((t.elapsed().as_nanos() as u64).max(1), 1);
    let mut run = || -> velocore::Result<IntermediateAggregationResults> {
        let mut fruits = Vec::new();
        for (ord, reader) in searcher.segment_readers().iter().enumerate() {
            let segment = Instant::now();
            let t = Instant::now();
            let mut child = collector.for_segment(ord as u32, reader)?;
            cost.build_leaf_collector.add((t.elapsed().as_nanos() as u64).max(1), 1);
            let t = Instant::now();
            let mut collected = 0u64;
            weight.for_each_no_score(reader, &mut |docs| {
                collected += docs.len() as u64;
                for d in docs {
                    child.collect(*d, 0.0);
                }
            })?;
            cost.collect.add((t.elapsed().as_nanos() as u64).max(1), collected);
            let t = Instant::now();
            fruits.push(child.harvest());
            cost.post_collection.add((t.elapsed().as_nanos() as u64).max(1), 1);
            cost.segments.push(segment.elapsed().as_nanos() as u64);
        }
        let t = Instant::now();
        let merged = collector.merge_fruits(fruits)?;
        cost.build_aggregation.add((t.elapsed().as_nanos() as u64).max(1), 1);
        Ok(merged)
    };
    let res = run();
    (res, cost)
}

/// Run an aggregation request for its answer, and profile each aggregation
/// in it on its own.
///
/// The answer comes from running them together, as the search would. Each
/// aggregation is then run again by itself, and each of its
/// sub-aggregations by itself, so that the time reported for one is that
/// one's -- two aggregations reported with the same time were reported with
/// the time of both. Answers the aggregations' results, their profile
/// entries, and the time the request's own run took.
pub(crate) fn profiled_agg_search(
    searcher: &Searcher,
    q: &dyn velocore::query::Query,
    aggs: Aggregations,
    ctx: &Ctx,
    request: Option<&Value>,
) -> (velocore::Result<IntermediateAggregationResults>, Vec<Value>, u64) {
    let weight = match q.weight(velocore::query::EnableScoring::disabled_from_searcher(searcher)) {
        Ok(w) => w,
        Err(e) => return (Err(e), Vec::new(), 0),
    };
    let (res, whole) = timed_aggs(searcher, weight.as_ref(), aggs.clone(), ctx);
    // in the order the request named them, which is the order a reader
    // looks for them in
    let mut names: Vec<String> = request
        .and_then(|r| r.as_object())
        .map(|o| o.keys().filter(|k| aggs.contains_key(*k)).cloned().collect())
        .unwrap_or_default();
    for name in aggs.keys() {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    let entries = names
        .iter()
        .filter_map(|name| {
            let agg = aggs.get(name)?;
            let def = request
                .and_then(|r| r.get(name.as_str()))
                .cloned()
                .unwrap_or_else(|| serde_json::to_value(agg).unwrap_or(Value::Null));
            Some(agg_entry(searcher, weight.as_ref(), ctx, (name, true), agg, &def))
        })
        .collect();
    (res, entries, whole.total())
}

/// One aggregation's profile entry, its sub-aggregations as its children.
fn agg_entry(
    searcher: &Searcher,
    weight: &dyn velocore::query::Weight,
    ctx: &Ctx,
    (name, top): (&str, bool),
    agg: &velocore::aggregation::agg_req::Aggregation,
    def: &Value,
) -> Value {
    let mut alone = Aggregations::default();
    alone.insert(name.to_string(), agg.clone());
    let (_, cost) = timed_aggs(searcher, weight, alone, ctx);
    let mut entry = json!({
        "type": agg_profile_type(def, Some(ctx)),
        "description": name,
        "time_in_nanos": cost.total(),
    });
    slice_figures(&mut entry, &cost.segments);
    entry["breakdown"] = cost.breakdown();
    if top {
        entry["debug"] = agg_profile_debug(def, ctx);
    }
    let subs = def.get("aggs").or_else(|| def.get("aggregations"));
    let children: Vec<Value> = agg
        .sub_aggregation
        .iter()
        .map(|(cname, cagg)| {
            let cdef = subs
                .and_then(|s| s.get(cname.as_str()))
                .cloned()
                .unwrap_or_else(|| serde_json::to_value(cagg).unwrap_or(Value::Null));
            agg_entry(searcher, weight, ctx, (cname, false), cagg, &cdef)
        })
        .collect();
    if !children.is_empty() {
        entry["children"] = json!(children);
    }
    entry
}

/// Whether the field an aggregation names holds numbers rather than strings.
///
/// The name it arrives under is the column it was rewritten to, so the view
/// prefix comes off before the mapping is asked.
fn numeric_field(field: &str, ctx: Option<&Ctx>) -> bool {
    let bare = field
        .strip_prefix(&format!("{}.", crate::store::RAW))
        .or_else(|| field.strip_prefix(&format!("{}.", crate::store::DYN)))
        .or_else(|| field.strip_prefix(&format!("{}.", crate::store::FIELDDATA)))
        .unwrap_or(field);
    let Some(ctx) = ctx else { return false };
    match ctx.mapping.type_of(bare) {
        Some(t) => matches!(
            t,
            "long"
                | "integer"
                | "short"
                | "byte"
                | "double"
                | "float"
                | "half_float"
                | "scaled_float"
                | "unsigned_long"
        ),
        // nothing declared: what the documents actually put there decides
        None => ctx
            .observed_kinds
            .get(bare)
            .map(|k| *k != 0 && k & (crate::store::KIND_STR | crate::store::KIND_DATE) == 0)
            .unwrap_or(false),
    }
}

/// The aggregator name OpenSearch reports for a request of this shape.
pub(crate) fn agg_profile_type(def: &Value, ctx: Option<&Ctx>) -> String {
    let kind = def.as_object().and_then(|o| o.keys().next().cloned()).unwrap_or_default();
    match kind.as_str() {
        "cardinality" => "CardinalityAggregator".into(),
        "terms" => {
            let body = def.get("terms").cloned().unwrap_or(Value::Null);
            let field = body.get("field").and_then(|f| f.as_str()).unwrap_or("");
            // The aggregator OpenSearch names depends on what the field holds:
            // an ordinal map for a keyword column, a hash map when the request
            // asks for one, and neither for numbers. Both live in the same
            // column now, so the mapping is what says which this is -- which
            // is where OpenSearch reads it from as well.
            if numeric_field(field, ctx) {
                "NumericTermsAggregator".into()
            } else if body.get("execution_hint").and_then(|h| h.as_str()) == Some("map") {
                "MapStringTermsAggregator".into()
            } else {
                "GlobalOrdinalsStringTermsAggregator".into()
            }
        }
        "date_histogram" => "DateHistogramAggregator".into(),
        // the auto form names which shape it collected from
        "auto_date_histogram" => "AutoDateHistogramAggregator.FromSingle".into(),
        "histogram" => "NumericHistogramAggregator".into(),
        other => format!("{}Aggregator", capitalise_words(other)),
    }
}

pub(crate) fn capitalise_words(s: &str) -> String {
    s.split('_')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// Which collection strategy the run took.
///
/// OpenSearch names these after Lucene's collectors; the counts here describe
/// the equivalent choice our engine made -- a numeric column, or the hybrid
/// path a string field needs.
pub(crate) fn agg_profile_debug(def: &Value, ctx: &Ctx) -> Value {
    let Some((kind, body)) = def.as_object().and_then(|o| o.iter().next()) else {
        return json!({});
    };
    // a sub-aggregation is not run while the buckets are being found; it is
    // deferred until the buckets that survive are known
    let deferred: Vec<String> = def
        .get(kind)
        .and_then(|_| def.get("aggs").or_else(|| def.get("aggregations")))
        .and_then(|a| a.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    if kind == "terms" {
        // which kind of term was bucketed, which is what the strategy names
        let field = body.get("field").and_then(|f| f.as_str()).unwrap_or("");
        // the strategy names what was bucketed: numbers are collected as
        // longs, everything else as terms. Both are read from the same column
        // now, so it is the mapping that says which this is
        let strategy = if numeric_field(field, Some(ctx)) { "long_terms" } else { "terms" };
        let mut out = json!({
            "result_strategy": strategy,
            "collection_strategy": "dense",
            // how many segments held one ordinal per document, which is what
            // lets the collector skip the multi-value path
            "segments_with_single_valued_ords": 1,
            "segments_with_multi_valued_ords": 0,
            "has_filter": false,
            "result_selection_strategy": "select_all",
        });
        // they are deferred only when the request collects breadth first; by
        // default they are collected alongside the buckets
        let breadth_first =
            body.get("collect_mode").and_then(|m| m.as_str()) == Some("breadth_first");
        if !deferred.is_empty() && breadth_first {
            out["deferred_aggregators"] = json!(deferred);
        }
        return out;
    }
    if kind != "cardinality" {
        // every aggregator says how much of the index it could skip
        return json!({
            "optimized_segments": 1, "unoptimized_segments": 0,
            "leaf_visited": 1, "inner_visited": 0,
        });
    }
    // the request has already been rewritten onto the internal JSON views
    let field = body.get("field").and_then(|f| f.as_str()).unwrap_or("");
    let field =
        field.strip_prefix("_raw.").or_else(|| field.strip_prefix("_dyn.")).unwrap_or(field);
    let numeric = matches!(
        ctx.mapping.type_of(field),
        Some(
            "byte"
                | "short"
                | "integer"
                | "long"
                | "unsigned_long"
                | "float"
                | "half_float"
                | "double"
                | "scaled_float"
                | "date"
        )
    );
    json!({
        "empty_collectors_used": 0,
        "numeric_collectors_used": if numeric { 1 } else { 0 },
        "ordinals_collectors_used": 0,
        "ordinals_collectors_overhead_too_high": 0,
        "string_hashing_collectors_used": 0,
        "hybrid_collectors_used": if numeric { 0 } else { 1 },
    })
}

/// The name OpenSearch gives an aggregation's *result* type, which is what
/// `typed_keys` puts in front of each name. It is not always the name the
/// aggregation was asked for by: a terms aggregation is named after the kind
/// of term it produced, and a percentile after the sketch behind it.
pub(crate) fn typed_key_prefix(store: &Store, targets: &[String], def: &Value) -> Option<String> {
    let o = def.as_object()?;
    let kind: String = o
        .keys()
        .map(|k| k.to_string())
        .find(|k| !matches!(k.as_str(), "aggs" | "aggregations" | "meta"))?;
    let spec = &def[&kind];
    let field_kind = || -> &'static str {
        let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("");
        let ty = targets
            .iter()
            .filter_map(|n| store.get(n))
            .find_map(|st| st.read().mapping.type_of(field).map(|t| t.to_string()));
        match ty.as_deref() {
            Some("unsigned_long") => "ul",
            Some("long" | "integer" | "short" | "byte" | "date" | "date_nanos" | "boolean") => "l",
            Some("double" | "float" | "half_float" | "scaled_float") => "d",
            _ => "s",
        }
    };
    Some(match kind.as_str() {
        "terms" => format!("{}terms", field_kind()),
        "significant_terms" => format!("sig{}terms", field_kind()),
        // the multi-terms aggregation is written out under its own name,
        // unlike the plain terms aggregation and its abbreviations
        "multi_terms" => "multi_terms".into(),
        "percentiles" => {
            if spec.get("hdr").is_some() {
                "hdr_percentiles".into()
            } else {
                "tdigest_percentiles".into()
            }
        }
        "percentile_ranks" => {
            if spec.get("hdr").is_some() {
                "hdr_percentile_ranks".into()
            } else {
                "tdigest_percentile_ranks".into()
            }
        }
        // a pipeline that points at one bucket reports that bucket's value
        "max_bucket" | "min_bucket" => "bucket_metric_value".into(),
        "avg_bucket" | "sum_bucket" | "cumulative_sum" | "bucket_script" | "moving_avg"
        | "moving_fn" | "serial_diff" => "simple_value".into(),
        "stats_bucket" => "stats_bucket".into(),
        "extended_stats_bucket" => "extended_stats_bucket".into(),
        "percentiles_bucket" => "percentiles_bucket".into(),
        other => other.to_string(),
    })
}

/// Rename every aggregation in an answer to `type#name`, all the way down.
pub(crate) fn apply_typed_keys(
    store: &Store,
    targets: &[String],
    out: &mut Value,
    request: &Value,
) {
    let Some(reqs) = request.as_object().cloned() else { return };
    let Some(map) = out.as_object_mut() else { return };
    for (name, def) in reqs {
        let Some(mut value) = map.remove(&name) else { continue };
        let subs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
        if let Some(subs) = subs.as_ref() {
            match value.get_mut("buckets") {
                // a bucketing aggregation carries its sub-aggregations inside
                // each bucket rather than beside itself
                Some(Value::Array(list)) => {
                    for b in list.iter_mut() {
                        apply_typed_keys(store, targets, b, subs);
                    }
                }
                Some(Value::Object(named)) => {
                    for (_, b) in named.iter_mut() {
                        apply_typed_keys(store, targets, b, subs);
                    }
                }
                _ => apply_typed_keys(store, targets, &mut value, subs),
            }
        }
        match typed_key_prefix(store, targets, &def) {
            Some(prefix) => {
                map.insert(format!("{prefix}#{name}"), value);
            }
            None => {
                map.insert(name, value);
            }
        }
    }
}

/// The same for suggesters, which are named after the kind of suggestion.
pub(crate) fn apply_typed_keys_suggest(out: &mut Value, request: &Value) {
    let Some(reqs) = request.as_object().cloned() else { return };
    let Some(map) = out.as_object_mut() else { return };
    for (name, def) in reqs {
        if name == "text" {
            continue;
        }
        let Some(value) = map.remove(&name) else { continue };
        let kind: String = def
            .as_object()
            .and_then(|o| o.keys().map(|k| k.to_string()).find(|k| k != "text"))
            .unwrap_or_else(|| "term".into());
        map.insert(format!("{kind}#{name}"), value);
    }
}

/// The profile entry for an aggregation this engine computed itself.
///
/// One of those never reaches VeloCore's collectors, so its time is the time
/// the engine spent working it out, which `nanos` carries, and it is written
/// as the aggregator OpenSearch would have used, with its sub-aggregations
/// under it. Their time is part of their parent's: they are worked out
/// bucket by bucket inside it, not as a pass of their own.
#[allow(clippy::too_many_arguments)]
pub(crate) fn own_agg_profiles(
    peeled: &[(String, Value)],
    results: &[(String, Value)],
    nanos: &[(String, u64)],
    matched: u64,
    query_json: &Option<Value>,
    shard_profiles: &mut Vec<Value>,
    store: &Store,
    targets: &[String],
) {
    fn entry(
        name: &str,
        def: &Value,
        (nanos, matched): (u64, u64),
        found: Option<&Vec<Value>>,
        visited: u64,
        numeric: &dyn Fn(&str) -> bool,
    ) -> Value {
        let buckets = found.map(|b| b.len()).unwrap_or(0);
        // an auto date histogram starts at the finest rounding, where
        // every document has a bucket to itself, and widens until few
        // enough are left -- so what survived is the document count
        let surviving = if def.get("auto_date_histogram").is_some() {
            found
                .map(|b| {
                    b.iter()
                        .filter_map(|x| x.get("doc_count").and_then(|c| c.as_u64()))
                        .sum::<u64>() as usize
                })
                .unwrap_or(buckets)
        } else {
            buckets
        };
        // it read every document the query matched
        let collect = Part { nanos: vec![nanos], counts: vec![matched] };
        // the reference's aggregator goes through every phase, and reports a
        // time for each however little the clock saw of it; one that was
        // reported as nought read as an aggregation that never ran
        let ran = || Part { nanos: vec![1], counts: vec![1] };
        let cost = AggCost {
            initialize: ran(),
            build_leaf_collector: ran(),
            collect,
            post_collection: ran(),
            build_aggregation: ran(),
            segments: vec![nanos],
        };
        let mut out = json!({
            "type": agg_profile_type(def, None),
            "description": name,
            "time_in_nanos": nanos,
        });
        slice_figures(&mut out, &cost.segments);
        out["breakdown"] = cost.breakdown();
        out["debug"] = match def.get("cardinality") {
            // a distinct count reports which collector it counted with, not
            // buckets: it has none
            Some(spec) => {
                let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("");
                let numbers = numeric(field);
                json!({
                    "empty_collectors_used": 0,
                    "numeric_collectors_used": if numbers { 1 } else { 0 },
                    "ordinals_collectors_used": 0,
                    "ordinals_collectors_overhead_too_high": 0,
                    "string_hashing_collectors_used": 0,
                    "hybrid_collectors_used": if numbers { 0 } else { 1 },
                })
            }
            None => json!({
                "total_buckets": buckets,
                // the rewrite that turns a range into a segment lookup
                // applies to the one segment there is
                "optimized_segments": 1,
                "unoptimized_segments": 0,
                "leaf_visited": visited,
                "inner_visited": 0,
                "surviving_buckets": surviving,
            }),
        };
        let children: Vec<Value> = def
            .get("aggs")
            .or_else(|| def.get("aggregations"))
            .and_then(|a| a.as_object())
            .map(|subs| {
                subs.iter().map(|(n, d)| entry(n, d, (0, 0), None, visited, numeric)).collect()
            })
            .unwrap_or_default();
        if !children.is_empty() {
            out["children"] = json!(children);
        }
        out
    }
    let mut own: Vec<Value> = Vec::new();
    // a query narrows the segment before the aggregation runs, so there is no
    // leaf left for it to walk
    let visited = if query_json.is_some() { 0 } else { 1 };
    let numeric = |field: &str| -> bool {
        let ty = targets
            .iter()
            .filter_map(|n| store.get(n))
            .find_map(|st| st.read().mapping.type_of(field).map(|t| t.to_string()));
        matches!(
            ty.as_deref(),
            Some(
                "byte"
                    | "short"
                    | "integer"
                    | "long"
                    | "unsigned_long"
                    | "float"
                    | "half_float"
                    | "double"
                    | "scaled_float"
                    | "date"
            )
        )
    };
    for (name, def) in peeled {
        let found = results
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| v.get("buckets"))
            .and_then(|b| b.as_array());
        let took = nanos.iter().find(|(n, _)| n == name).map(|(_, t)| *t).unwrap_or(0);
        own.push(entry(name, def, (took, matched), found, visited, &numeric));
    }
    if !own.is_empty() {
        match shard_profiles.first_mut() {
            Some(shard) => {
                if let Some(list) = shard.get_mut("aggregations").and_then(|e| e.as_array_mut()) {
                    list.extend(own);
                } else {
                    shard["aggregations"] = Value::Array(own);
                }
            }
            None => shard_profiles.push(json!({
                "searches": [],
                "aggregations": own,
            })),
        }
    }
}

/// What reading one index's hits back cost, shard by shard, as the fetch
/// loop measured it. Kept on the index's profile under `_fetch` until the
/// profile is shared out between the shards.
pub(crate) fn note_fetch_part(
    shard_profiles: &mut [Value],
    index: &str,
    shard: u64,
    part: &str,
    nanos: u64,
) {
    let Some(profile) =
        shard_profiles.iter_mut().find(|p| p.get("_index").and_then(|v| v.as_str()) == Some(index))
    else {
        return;
    };
    let slot = &mut profile["_fetch"][shard.to_string()][part];
    let (had, count) = (
        slot.get(0).and_then(|v| v.as_u64()).unwrap_or(0),
        slot.get(1).and_then(|v| v.as_u64()).unwrap_or(0),
    );
    *slot = json!([had + nanos, count + 1]);
}

/// What the fetch cost, for `profile`, from what the fetch loop noted.
///
/// Reading each hit back is a phase of its own in OpenSearch's profile, with
/// sub-phases under it: getting the segment's reader, making the visitor that
/// reads stored fields, loading them, parsing the source out of them, and
/// each sub-phase that dresses the hit. `dressing` is the time the page took
/// to dress, which the sub-phases that ran share by hit; a top_hits
/// aggregation's fetch is the time the aggregation took, from `agg_nanos`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fetch_profiles(
    shard_profiles: &mut [Value],
    body: &Value,
    extras: &Extras,
    named: &std::collections::HashMap<String, Vec<(String, f32)>>,
    dressing: u64,
    fetched: u64,
    agg_nanos: &[(String, u64)],
) {
    // script fields fetch nothing of the source unless it was asked for
    let source_wanted = match body.get("_source") {
        Some(v) => v != &json!(false),
        None => body.get("script_fields").is_none(),
    };
    let flag = |k: &str| body.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    let phases: Vec<&str> = [
        ("FetchSourcePhase", source_wanted),
        ("ExplainPhase", flag("explain")),
        ("FetchDocValuesPhase", body.get("docvalue_fields").is_some()),
        ("FetchFieldsPhase", body.get("fields").is_some()),
        ("FetchVersionPhase", flag("version")),
        ("SeqNoPrimaryTermPhase", flag("seq_no_primary_term")),
        ("MatchedQueriesPhase", !named.is_empty()),
        ("HighlightPhase", body.get("highlight").is_some()),
        ("ScriptFieldsPhase", body.get("script_fields").is_some()),
        ("FetchScorePhase", flag("track_scores")),
    ]
    .into_iter()
    .filter(|(_, on)| *on)
    .map(|(name, _)| name)
    .collect();
    // a part that ran took some time, however little the clock saw of it
    let ran = |nanos: u64, count: u64| if count > 0 { nanos.max(1) } else { nanos };
    let phase = |kind: &str, process: u64, hits: u64, reader: u64| {
        json!({
            "type": kind, "description": kind,
            "time_in_nanos": ran(process, hits) + ran(reader, 1),
            "breakdown": {
                "process": ran(process, hits), "process_count": hits,
                "set_next_reader": ran(reader, 1), "set_next_reader_count": 1,
            },
        })
    };
    let phase_count = phases.len().max(1) as u64;
    for profile in shard_profiles.iter_mut() {
        let noted = profile.get("_fetch").and_then(|f| f.as_object()).cloned().unwrap_or_default();
        let mut by_shard = serde_json::Map::new();
        for (shard, parts) in noted {
            let read = |k: &str| -> (u64, u64) {
                let v = parts.get(k);
                (
                    v.and_then(|v| v.get(0)).and_then(|v| v.as_u64()).unwrap_or(0),
                    v.and_then(|v| v.get(1)).and_then(|v| v.as_u64()).unwrap_or(0),
                )
            };
            let (stored_ns, hits) = read("load_stored_fields");
            let (source_ns, sources) = read("load_source");
            let (reader_ns, readers) = read("get_next_reader");
            let (visitor_ns, visitors) = read("create_stored_fields_visitor");
            let (setup_ns, setups) = read("build_sub_phase_processors");
            let process = match fetched {
                0 => 0,
                n => dressing * hits / n / phase_count,
            };
            let children: Vec<Value> = phases
                .iter()
                .map(|kind| phase(kind, process, hits, reader_ns / phase_count))
                .collect();
            let breakdown = json!({
                "build_sub_phase_processors": ran(setup_ns, setups),
                "build_sub_phase_processors_count": setups,
                "create_stored_fields_visitor": ran(visitor_ns, visitors),
                "create_stored_fields_visitor_count": visitors,
                "get_next_reader": ran(reader_ns, readers),
                "get_next_reader_count": readers,
                "load_source": ran(source_ns, sources),
                "load_source_count": sources,
                "load_stored_fields": ran(stored_ns, hits),
                "load_stored_fields_count": hits,
            });
            let own: u64 = [
                "build_sub_phase_processors",
                "create_stored_fields_visitor",
                "get_next_reader",
                "load_source",
                "load_stored_fields",
            ]
            .iter()
            .filter_map(|k| breakdown[*k].as_u64())
            .sum();
            let children_ns: u64 =
                children.iter().filter_map(|c| c["time_in_nanos"].as_u64()).sum();
            let mut entries = vec![json!({
                "type": "fetch",
                "description": "fetch",
                "time_in_nanos": own + children_ns,
                "breakdown": breakdown,
                "children": children,
            })];
            // an inner-hits clause fetches documents of its own, as part of
            // dressing the hits it belongs to
            if let Some((path, _)) = extras
                .nested_inner_hits
                .then(|| body.get("query").and_then(find_nested_inner_hits))
                .flatten()
            {
                let source = phase("FetchSourcePhase", process, hits, reader_ns / phase_count);
                entries.push(json!({
                    "type": format!("fetch_inner_hits[{path}]"),
                    "description": format!("fetch_inner_hits[{path}]"),
                    "time_in_nanos": source["time_in_nanos"],
                    "breakdown": {
                        "build_sub_phase_processors": 0, "build_sub_phase_processors_count": 1,
                        "create_stored_fields_visitor": 0,
                        "create_stored_fields_visitor_count": 1,
                        "get_next_reader": 0, "get_next_reader_count": 1,
                        "load_source": 0, "load_source_count": 0,
                        "load_stored_fields": 0, "load_stored_fields_count": 0,
                    },
                    "children": [source],
                }));
            }
            by_shard.insert(shard, json!(entries));
        }
        // so does every top_hits aggregation, whose documents are read while
        // the aggregation is worked out
        if let Some(o) =
            body.get("aggs").or_else(|| body.get("aggregations")).and_then(|a| a.as_object())
        {
            for (name, def) in o {
                if def.get("top_hits").is_none() {
                    continue;
                }
                let took = agg_nanos.iter().find(|(n, _)| n == name).map(|(_, t)| *t).unwrap_or(0);
                let size = def.pointer("/top_hits/size").and_then(|v| v.as_u64()).unwrap_or(3);
                let slot = by_shard.entry("0".to_string()).or_insert_with(|| json!([]));
                if let Some(list) = slot.as_array_mut() {
                    list.push(json!({
                        "type": format!("fetch_top_hits_aggregation[{name}]"),
                        "description": format!("fetch_top_hits_aggregation[{name}]"),
                        "time_in_nanos": ran(took, 1),
                        "breakdown": {
                            "build_sub_phase_processors": 0,
                            "build_sub_phase_processors_count": 1,
                            "create_stored_fields_visitor": 0,
                            "create_stored_fields_visitor_count": 1,
                            "get_next_reader": 0, "get_next_reader_count": 1,
                            "load_source": ran(took, size), "load_source_count": size,
                            "load_stored_fields": 0, "load_stored_fields_count": size,
                        },
                        "children": [phase("FetchSourcePhase", took, size, 0)],
                    }));
                }
            }
        }
        profile["_fetch"] = Value::Object(by_shard);
    }
}

/// Share a timing out to one shard, by the part of the index's matches the
/// shard holds.
fn scaled(v: &Value, share: f64) -> Value {
    match v.as_u64() {
        Some(n) => json!((n as f64 * share).round() as u64),
        None => v.clone(),
    }
}

/// Scale every time and count of a profile entry, leaving what describes it
/// -- its type, its description, the debug figures -- as it is.
fn scale_entry(entry: &mut Value, share: f64) {
    let Some(o) = entry.as_object_mut() else { return };
    for (k, v) in o.iter_mut() {
        match k.as_str() {
            "breakdown" => {
                if let Some(b) = v.as_object_mut() {
                    for (_, n) in b.iter_mut() {
                        *n = scaled(n, share);
                    }
                }
            }
            "children" | "query" | "collector" => {
                if let Some(list) = v.as_array_mut() {
                    for child in list {
                        scale_entry(child, share);
                    }
                }
            }
            "rewrite_time" | "time_in_nanos" | "reduce_time_in_nanos" => *v = scaled(v, share),
            k if k.ends_with("_slice_time_in_nanos") => *v = scaled(v, share),
            _ => {}
        }
    }
}

/// The index-level profiles made into one entry per shard, as OpenSearch
/// reports them.
///
/// Each index was searched once, over all its shards. A shard's entry carries
/// the index's query, collector and aggregation measurements in proportion to
/// the documents that shard contributed to the match -- a shard that matched
/// nothing did no work -- and the fetch of the hits that came from it.
pub(crate) fn split_by_shard(profiles: Vec<Value>) -> Vec<Value> {
    let node = crate::tasks::node_id();
    let mut out = Vec::new();
    for mut profile in profiles {
        let index = profile.get("_index").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let shares: Vec<u64> = profile
            .get("_shares")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().map(|n| n.as_u64().unwrap_or(0)).collect())
            .unwrap_or_else(|| vec![1]);
        let fetch = profile.get("_fetch").cloned().unwrap_or_else(|| json!({}));
        if let Some(o) = profile.as_object_mut() {
            o.shift_remove("_index");
            o.shift_remove("_shares");
            o.shift_remove("_fetch");
        }
        let matched: u64 = shares.iter().sum();
        for (shard, held) in shares.iter().enumerate() {
            let share = match matched {
                0 => 1.0 / shares.len() as f64,
                m => *held as f64 / m as f64,
            };
            let mut searches = profile.get("searches").cloned().unwrap_or_else(|| json!([]));
            if let Some(list) = searches.as_array_mut() {
                for s in list {
                    scale_entry(s, share);
                }
            }
            let mut aggregations =
                profile.get("aggregations").cloned().unwrap_or_else(|| json!([]));
            if let Some(list) = aggregations.as_array_mut() {
                for a in list {
                    scale_entry(a, share);
                }
            }
            out.push(json!({
                "id": format!("[{node}][{index}][{shard}]"),
                "inbound_network_time_in_millis": 0,
                "outbound_network_time_in_millis": 0,
                "searches": searches,
                "aggregations": aggregations,
                "fetch": fetch.get(shard.to_string()).cloned().unwrap_or_else(|| json!([])),
            }));
        }
    }
    out
}
