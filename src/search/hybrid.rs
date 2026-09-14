//! The `hybrid` query: several queries asked side by side, each one's scores
//! put on a common scale by a search pipeline and added up into one.
//!
//! This is the neural-search plugin's query, and it is answered the way the
//! plugin answers it. Every shard collects each sub-query's best documents on
//! its own; the pipeline's `normalization-processor` (or
//! `score-ranker-processor`) scales each sub-query's scores over every shard's
//! lists, combines the scaled scores document by document, and only then is a
//! page taken. An index here is one shard, so "per shard" is "per index".
//!
//! The parts are ordinary searches: one per index per sub-query for the lists,
//! one over the union for the total and the aggregations, and one that reads
//! the documents on the page.

use std::collections::HashMap;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use crate::api::{Params, err};
use crate::search::pipeline::{Config, PipelineError};
use crate::store::Store;

/// The most sub-queries one `hybrid` query may hold.
const MOST_SUB_QUERIES: usize = 5;
/// A sub-query whose documents all scored the same gives each of them this.
const SINGLE_RESULT_SCORE: f32 = 1.0;
/// A scaled score of nothing still says the document was found.
const MIN_SCORE: f32 = 0.001;
/// What the plugin writes around and between the sub-query lists when no
/// pipeline takes them apart: a search without a normalization processor
/// answers with these as scores.
const MAGIC_START_STOP: f32 = -9_549_511_920.488_16;
const MAGIC_DELIMITER: f32 = -4_422_440_593.979_12;

/// How a lower bound treats a score below it.
#[derive(Clone, Copy, PartialEq)]
enum BoundMode {
    Apply,
    Clip,
    Ignore,
}

#[derive(Clone, Copy)]
struct LowerBound {
    mode: BoundMode,
    min_score: f32,
}

#[derive(Clone)]
enum Normalization {
    MinMax(Vec<LowerBound>),
    L2,
    ZScore,
    Rrf(u32),
}

#[derive(Clone, Copy, PartialEq)]
enum Combination {
    Arithmetic,
    Geometric,
    Harmonic,
    Rrf,
}

/// What a phase results processor does to a hybrid query's scores.
#[derive(Clone)]
pub struct Scoring {
    normalization: Normalization,
    combination: Combination,
    weights: Vec<f32>,
}

fn illegal(reason: impl Into<String>) -> PipelineError {
    PipelineError::of("illegal_argument_exception", reason)
}

fn combination_named(name: &str) -> Option<Combination> {
    match name {
        "arithmetic_mean" => Some(Combination::Arithmetic),
        "geometric_mean" => Some(Combination::Geometric),
        "harmonic_mean" => Some(Combination::Harmonic),
        "rrf" => Some(Combination::Rrf),
        _ => None,
    }
}

/// The `parameters` of a combination: `weights` and nothing else.
fn read_weights(params: Option<Map<String, Value>>) -> Result<Vec<f32>, PipelineError> {
    let Some(params) = params else { return Ok(Vec::new()) };
    if params.keys().any(|k| k != "weights") {
        return Err(illegal(
            "provided parameter for combination technique is not supported. supported \
             parameters are [weights]",
        ));
    }
    let Some(raw) = params.get("weights") else { return Ok(Vec::new()) };
    let Some(list) = raw.as_array() else {
        return Err(illegal("parameter [weights] must be a collection of numbers"));
    };
    let mut weights = Vec::with_capacity(list.len());
    for w in list {
        match w {
            // the plugin reads the list as doubles, and a whole number in it is
            // an Integer the cast cannot turn into one
            Value::Number(n) if n.is_f64() => weights.push(n.as_f64().unwrap_or(0.0) as f32),
            Value::Number(_) => {
                return Err(PipelineError::of(
                    "class_cast_exception",
                    "class java.lang.Integer cannot be cast to class java.lang.Double \
                     (java.lang.Integer and java.lang.Double are in module java.base of \
                     loader 'bootstrap')",
                ));
            }
            _ => return Err(illegal("parameter [weights] must be a collection of numbers")),
        }
    }
    let shown = weights.iter().map(|w| crate::search::pipeline::java_float(*w)).collect::<Vec<_>>();
    if weights.iter().any(|w| !(0.0..=1.0).contains(w)) {
        return Err(illegal(format!(
            "all weights must be in range [0.0 ... 1.0], submitted weights: [{}]",
            shown.join(", ")
        )));
    }
    let sum: f32 = weights.iter().sum();
    if (sum - 1.0).abs() > 0.01 + f32::EPSILON * 4.0 {
        return Err(illegal(format!(
            "sum of weights for combination must be equal to 1.0, submitted weights: [{}]",
            shown.join(", ")
        )));
    }
    Ok(weights)
}

/// `lower_bounds`, the one parameter `min_max` takes.
fn read_bounds(params: Option<Map<String, Value>>) -> Result<Vec<LowerBound>, PipelineError> {
    let unrecognized = || illegal("unrecognized parameters in normalization technique");
    let Some(params) = params else { return Ok(Vec::new()) };
    if params.keys().any(|k| k != "lower_bounds") {
        return Err(unrecognized());
    }
    let Some(raw) = params.get("lower_bounds") else { return Ok(Vec::new()) };
    let Some(list) = raw.as_array() else { return Err(illegal("lower_bounds must be a List")) };
    if list.len() > MOST_SUB_QUERIES {
        return Err(illegal(format!(
            "lower_bounds size {} should be less than or equal to {MOST_SUB_QUERIES}",
            list.len()
        )));
    }
    let mut bounds = Vec::with_capacity(list.len());
    for item in list {
        let Some(o) = item.as_object() else { return Err(unrecognized()) };
        if o.keys().any(|k| k != "mode" && k != "min_score") {
            return Err(unrecognized());
        }
        let mode = match o.get("mode").map(|m| m.as_str().map(str::to_string)) {
            None => BoundMode::Apply,
            Some(Some(m)) => match m.to_lowercase().as_str() {
                "apply" => BoundMode::Apply,
                "clip" => BoundMode::Clip,
                "ignore" => BoundMode::Ignore,
                _ => {
                    return Err(illegal(format!(
                        "invalid mode: {m}, valid values are: apply, clip, ignore"
                    )));
                }
            },
            Some(None) => {
                return Err(illegal("invalid mode, valid values are: apply, clip, ignore"));
            }
        };
        let min_score = match o.get("min_score") {
            None => 0.0,
            Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
            Some(Value::String(s)) => match s.trim().parse::<f64>() {
                Ok(v) => v,
                Err(_) => {
                    let mut e =
                        illegal("invalid format for min_score: must be a valid float value");
                    e.caused_by = Some(json!({
                        "type": "number_format_exception",
                        "reason": format!("For input string: \"{s}\""),
                    }));
                    return Err(e);
                }
            },
            Some(_) => {
                return Err(illegal("invalid format for min_score: must be a valid float value"));
            }
        };
        if !min_score.is_finite() || !(-10_000.0..=10_000.0).contains(&min_score) {
            return Err(illegal(
                "min_score must be a valid finite number between -10000.000000 and 10000.000000",
            ));
        }
        bounds.push(LowerBound { mode, min_score: min_score as f32 });
    }
    Ok(bounds)
}

/// A `normalization-processor` as written in a pipeline.
pub(crate) fn parse_normalization(cfg: &mut Config) -> Result<Scoring, PipelineError> {
    let mut normalization = Normalization::MinMax(Vec::new());
    let mut norm_name = "min_max".to_string();
    if let Some(mut clause) = cfg.opt_map("normalization")? {
        let mut inner = cfg.nested(&mut clause);
        norm_name = inner.string_or("technique", "min_max")?;
        let params = inner.opt_map("parameters")?;
        normalization = match norm_name.as_str() {
            "min_max" => Normalization::MinMax(read_bounds(params)?),
            "l2" | "z_score" => {
                if params.is_some_and(|p| !p.is_empty()) {
                    return Err(illegal("unrecognized parameters in normalization technique"));
                }
                if norm_name == "l2" { Normalization::L2 } else { Normalization::ZScore }
            }
            "rrf" => Normalization::Rrf(60),
            _ => return Err(illegal("provided normalization technique is not supported")),
        };
    }
    let mut combination = Combination::Arithmetic;
    let mut comb_name = "arithmetic_mean".to_string();
    let mut weights = Vec::new();
    if let Some(mut clause) = cfg.opt_map("combination")? {
        let mut inner = cfg.nested(&mut clause);
        comb_name = inner.string_or("technique", "arithmetic_mean")?;
        let params = inner.opt_map("parameters")?;
        combination = combination_named(&comb_name)
            .ok_or_else(|| illegal("provided combination technique is not supported"))?;
        weights = read_weights(params)?;
    }
    // which combinations make sense depends on what the scores were scaled to
    let allowed: &[&str] = match normalization {
        Normalization::ZScore => &["arithmetic_mean"],
        Normalization::MinMax(_) | Normalization::L2 => {
            &["harmonic_mean", "arithmetic_mean", "geometric_mean"]
        }
        Normalization::Rrf(_) => &["harmonic_mean", "arithmetic_mean", "geometric_mean", "rrf"],
    };
    if !allowed.contains(&comb_name.as_str()) {
        return Err(illegal(format!(
            "provided combination technique {comb_name} is not supported for normalization \
             technique {norm_name}. Supported techniques are: {}",
            allowed.join(", ")
        )));
    }
    Ok(Scoring { normalization, combination, weights })
}

/// A `score-ranker-processor`: reciprocal rank fusion.
pub(crate) fn parse_score_ranker(cfg: &mut Config) -> Result<Scoring, PipelineError> {
    let mut rank_constant = 60u32;
    let mut combination = Combination::Rrf;
    let mut weights = Vec::new();
    if let Some(mut clause) = cfg.opt_map("combination")? {
        let mut inner = cfg.nested(&mut clause);
        let name = inner.string_or("technique", "rrf")?;
        combination = combination_named(&name)
            .ok_or_else(|| illegal("provided combination technique is not supported"))?;
        if let Some(rc) = clause.get("rank_constant") {
            let n = rc
                .as_i64()
                .filter(|_| rc.is_i64() || rc.is_u64())
                .ok_or_else(|| illegal("parameter [rank_constant] must be an integer"))?;
            if !(1..=10_000).contains(&n) {
                return Err(illegal(format!(
                    "rank constant must be in the interval between 1 and 10000, submitted rank \
                     constant: {n}"
                )));
            }
            rank_constant = n as u32;
        }
        weights = read_weights(clause.get("parameters").and_then(|p| p.as_object()).cloned())?;
    }
    Ok(Scoring { normalization: Normalization::Rrf(rank_constant), combination, weights })
}

/// A `hybrid` query, read.
pub(crate) struct Hybrid {
    /// the clause as written
    raw: Value,
    queries: Vec<Value>,
    filter: Option<Value>,
    depth: Option<i64>,
    name: Option<String>,
}

fn parsing(reason: &str) -> Response {
    err(StatusCode::BAD_REQUEST, "parsing_exception", reason.to_string())
}

/// Read a `hybrid` clause's options, refusing what the plugin refuses when it
/// parses one.
fn read(h: &Value) -> Result<Hybrid, String> {
    let Some(o) = h.as_object() else {
        return Err("[hybrid] query malformed, no start_object after query name".into());
    };
    let mut queries = None;
    let mut filter = None;
    let mut depth = None;
    let mut name = None;
    for (k, v) in o {
        match k.as_str() {
            "queries" => queries = Some(v),
            "filter" => {
                if !v.is_object() {
                    return Err("[hybrid] query's [filter] field must be a query object".into());
                }
                filter = Some(v.clone());
            }
            "pagination_depth" => depth = v.as_i64(),
            "_name" => name = v.as_str().map(str::to_string),
            "boost" => return Err("[hybrid] query does not support [boost]".into()),
            _ => return Err("Field is not supported by [hybrid] query".into()),
        }
    }
    let queries: Vec<Value> = match queries {
        None => Vec::new(),
        Some(Value::Array(a)) => {
            if a.iter().any(|q| !q.is_object()) || a.is_empty() {
                return Err("[_na] query malformed, must start with start_object".into());
            }
            a.clone()
        }
        Some(q @ Value::Object(_)) => vec![q.clone()],
        Some(_) => return Err("[_na] query malformed, must start with start_object".into()),
    };
    if queries.is_empty() {
        return Err("[hybrid] requires 'queries' field with at least one clause".into());
    }
    if queries.len() > MOST_SUB_QUERIES {
        return Err("Number of sub-queries exceeds maximum supported by [hybrid] query".into());
    }
    Ok(Hybrid { raw: h.clone(), queries, filter, depth, name })
}

/// The `hybrid` clause as an ordinary query: every document any sub-query
/// matches, scored by the sum of what each gave it. This is what a hybrid
/// query counts as where no pipeline takes its scores apart -- `_count`, or
/// under a `bool` that only filters.
pub(crate) fn as_bool(h: &Value) -> Result<Value, String> {
    let spec = read(h)?;
    let mut b = json!({"should": spec.queries, "minimum_should_match": 1});
    if let Some(f) = spec.filter {
        b["filter"] = json!([f]);
    }
    if let Some(n) = spec.name {
        b["_name"] = json!(n);
    }
    Ok(json!({ "bool": b }))
}

/// Whether a query holds a `hybrid` clause anywhere: the name, with the
/// `queries` only that clause takes.
fn holds_hybrid(q: &Value) -> bool {
    match q {
        Value::Object(o) => {
            o.iter().any(|(k, v)| (k == "hybrid" && v.get("queries").is_some()) || holds_hybrid(v))
        }
        Value::Array(a) => a.iter().any(holds_hybrid),
        _ => false,
    }
}

/// The clauses a `bool` lists under one occurrence.
fn clauses(v: Option<&Value>) -> Vec<&Value> {
    match v {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(o @ Value::Object(_)) => vec![o],
        _ => Vec::new(),
    }
}

/// The failure a shard reports, with the chain of causes the reference
/// prints for it.
fn shard_failure(index: &str, cause: Value) -> Response {
    let mut root = cause.clone();
    if let Some(o) = root.as_object_mut() {
        o.remove("caused_by");
    }
    let mut outer = cause.clone();
    if outer.get("caused_by").is_none() {
        outer["caused_by"] = root.clone();
    }
    let body = json!({
        "error": {
            "root_cause": [root],
            "type": "search_phase_execution_exception",
            "reason": "all shards failed",
            "phase": "query",
            "grouped": true,
            "failed_shards": [{"shard": 0, "index": index, "node": "node0", "reason": cause}],
            "caused_by": outer,
        },
        "status": 400,
    });
    (StatusCode::BAD_REQUEST, axum::Json(body)).into_response()
}

fn shard_illegal(index: &str, reason: &str) -> Response {
    shard_failure(index, json!({"type": "illegal_argument_exception", "reason": reason}))
}

/// A failure of the step that puts the shards' lists together.
fn phase_failure(reason: &str) -> Response {
    let body = json!({
        "error": {
            "root_cause": [],
            "type": "search_phase_execution_exception",
            "reason": "The phase has failed",
            "phase": "query",
            "grouped": true,
            "failed_shards": [],
            "caused_by": {"type": "illegal_argument_exception", "reason": reason},
        },
        "status": 400,
    });
    (StatusCode::BAD_REQUEST, axum::Json(body)).into_response()
}

const WRAPPED: &str = "hybrid query must be a top level query and cannot be wrapped into other \
                       queries. To use scoring wrapper queries (function_score, script_score, \
                       etc.) with hybrid sub-queries, replace the hybrid clause with a bool query \
                       using should clauses containing the same sub-queries";

/// The hybrid query a search body asks for, if it asks for one the way the
/// plugin takes one: at the top, or as the only clause of a `bool`.
///
/// A `bool` that holds one in `must` beside clauses that only filter is an
/// ordinary query, and the hybrid clause in it counts as the sum of its parts;
/// any other place is refused.
pub(crate) fn plan(store: &Store, expr: &str, body: &Value) -> Result<Option<Hybrid>, Response> {
    let Some(q) = body.get("query") else { return Ok(None) };
    if !holds_hybrid(q) {
        return Ok(None);
    }
    let first_index = store.resolve(expr).into_iter().next().unwrap_or_default();
    let top = if let Some(h) = q.get("hybrid") {
        Some(h)
    } else if let Some(b) = q.get("bool").and_then(|b| b.as_object()) {
        let must = clauses(b.get("must"));
        let should = clauses(b.get("should"));
        let others_only_filter =
            b.keys().all(|k| matches!(k.as_str(), "must" | "filter" | "must_not"));
        if b.len() == 1 && must.len() + should.len() == 1 {
            must.first().or(should.first()).and_then(|c| c.get("hybrid"))
        } else if others_only_filter
            && must.len() == 1
            && must[0].get("hybrid").is_some()
            && !clauses(b.get("filter"))
                .iter()
                .chain(clauses(b.get("must_not")).iter())
                .any(|c| holds_hybrid(c))
        {
            // an ordinary search, the hybrid clause read as a sum
            if let Err(e) = as_bool(&must[0]["hybrid"]) {
                return Err(parsing(&e));
            }
            return Ok(None);
        } else {
            None
        }
    } else {
        None
    };
    let Some(h) = top else { return Err(shard_illegal(&first_index, WRAPPED)) };
    let spec = read(h).map_err(|e| parsing(&e))?;
    if spec.queries.iter().any(holds_hybrid) {
        return Err(shard_illegal(
            &first_index,
            "hybrid query cannot be nested in another hybrid query",
        ));
    }
    if spec.filter.as_ref().is_some_and(holds_hybrid) {
        return Err(shard_illegal(&first_index, WRAPPED));
    }
    Ok(Some(spec))
}

/// One document one sub-query found on one shard.
#[derive(Clone)]
struct Found {
    index: String,
    id: String,
    /// where the document sits in its shard: its place in the index's write
    /// order, which is the order Lucene numbers the documents of a shard
    /// written in one go
    doc: u64,
    score: f32,
}

/// The order a `java.util.HashMap` keyed by these document numbers hands its
/// keys back in. The plugin combines scores through such a map and sorts its
/// entries by score with a stable sort, so documents that tie keep this order.
fn java_hash_order(docs: &[u64]) -> Vec<usize> {
    let mut cap = 16usize;
    while docs.len() as f64 > cap as f64 * 0.75 {
        cap *= 2;
    }
    let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); cap];
    for (i, d) in docs.iter().enumerate() {
        let h = *d as u32;
        let h = h ^ (h >> 16);
        buckets[(h as usize) & (cap - 1)].push(i);
    }
    buckets.into_iter().flatten().collect()
}

/// Scale each sub-query's scores, over every shard's list for it.
fn normalize(scoring: &Scoring, shards: &mut [Vec<Vec<Found>>], subs: usize) {
    for j in 0..subs {
        let all = || shards.iter().flat_map(|s| s.get(j).into_iter().flatten());
        match &scoring.normalization {
            Normalization::MinMax(bounds) => {
                let lo = all().map(|f| f.score).fold(f32::INFINITY, f32::min);
                let hi = all().map(|f| f.score).fold(f32::NEG_INFINITY, f32::max);
                let bound = bounds.get(j).copied();
                for f in shards.iter_mut().filter_map(|s| s.get_mut(j)).flatten() {
                    f.score = min_max(f.score, lo, hi, bound);
                }
            }
            Normalization::L2 => {
                let sum: f32 = all().map(|f| f.score * f.score).sum();
                let norm = sum.sqrt();
                for f in shards.iter_mut().filter_map(|s| s.get_mut(j)).flatten() {
                    f.score = if norm == 0.0 { MIN_SCORE } else { f.score / norm };
                }
            }
            Normalization::ZScore => {
                let n = all().count();
                let mean = all().map(|f| f.score as f64).sum::<f64>() / n.max(1) as f64;
                let var = all().map(|f| (f.score as f64 - mean).powi(2)).sum::<f64>()
                    / (n.saturating_sub(1)).max(1) as f64;
                let sd = var.sqrt();
                for f in shards.iter_mut().filter_map(|s| s.get_mut(j)).flatten() {
                    f.score = if n < 2 || sd == 0.0 {
                        SINGLE_RESULT_SCORE
                    } else {
                        let z = ((f.score as f64 - mean) / sd) as f32;
                        if z <= 0.0 { MIN_SCORE } else { z }
                    };
                }
            }
            Normalization::Rrf(k) => {
                // a rank is a place in this shard's own list
                for s in shards.iter_mut() {
                    if let Some(list) = s.get_mut(j) {
                        for (rank, f) in list.iter_mut().enumerate() {
                            f.score = (1.0 / (*k as f64 + rank as f64 + 1.0)) as f32;
                        }
                    }
                }
            }
        }
    }
}

fn min_max(score: f32, lo: f32, hi: f32, bound: Option<LowerBound>) -> f32 {
    if hi == lo && hi == score {
        return SINGLE_RESULT_SCORE;
    }
    let plain = || {
        let v = (score - lo) / (hi - lo);
        if v == 0.0 { MIN_SCORE } else { v }
    };
    let Some(b) = bound.filter(|b| b.mode != BoundMode::Ignore) else { return plain() };
    // a bound above everything found cannot say anything about the scores
    if hi < b.min_score {
        return plain();
    }
    if score < b.min_score {
        return match b.mode {
            BoundMode::Clip => MIN_SCORE,
            _ => plain(),
        };
    }
    let v = (score - b.min_score) / (hi - b.min_score);
    if v == 0.0 { MIN_SCORE } else { v }
}

fn combine(how: Combination, weights: &[f32], scores: &[f32]) -> f32 {
    let w = |i: usize| weights.get(i).copied().unwrap_or(1.0);
    match how {
        Combination::Arithmetic => {
            let (mut total, mut sum_w) = (0.0f32, 0.0f32);
            for (i, s) in scores.iter().enumerate() {
                if *s >= 0.0 {
                    total += s * w(i);
                    sum_w += w(i);
                }
            }
            if sum_w == 0.0 { 0.0 } else { total / sum_w }
        }
        Combination::Geometric => {
            let (mut logs, mut sum_w) = (0.0f64, 0.0f64);
            for (i, s) in scores.iter().enumerate() {
                if *s > 0.0 {
                    logs += w(i) as f64 * (*s as f64).ln();
                    sum_w += w(i) as f64;
                }
            }
            if sum_w == 0.0 { 0.0 } else { (logs / sum_w).exp() as f32 }
        }
        Combination::Harmonic => {
            let (mut harmonics, mut sum_w) = (0.0f32, 0.0f32);
            for (i, s) in scores.iter().enumerate() {
                if *s > 0.0 {
                    harmonics += w(i) / s;
                    sum_w += w(i);
                }
            }
            if harmonics > 0.0 { sum_w / harmonics } else { 0.0 }
        }
        Combination::Rrf => scores.iter().enumerate().map(|(i, s)| s * w(i)).sum(),
    }
}

/// Every shard's documents with their combined scores, best first, shard by
/// shard where they tie.
fn combine_shards(scoring: &Scoring, shards: &[Vec<Vec<Found>>], subs: usize) -> Vec<Found> {
    let mut merged: Vec<Found> = Vec::new();
    for shard in shards {
        let mut docs: Vec<Found> = Vec::new();
        let mut scores: Vec<Vec<f32>> = Vec::new();
        let mut at: HashMap<&str, usize> = HashMap::new();
        for (j, list) in shard.iter().enumerate() {
            for f in list {
                let slot = *at.entry(f.id.as_str()).or_insert_with(|| {
                    docs.push(f.clone());
                    scores.push(vec![0.0; subs]);
                    docs.len() - 1
                });
                scores[slot][j] = f.score;
            }
        }
        let numbers: Vec<u64> = docs.iter().map(|d| d.doc).collect();
        let mut order = java_hash_order(&numbers);
        let combined: Vec<f32> =
            scores.iter().map(|s| combine(scoring.combination, &scoring.weights, s)).collect();
        order.sort_by(|a, b| combined[*b].total_cmp(&combined[*a]));
        for i in order {
            let mut f = docs[i].clone();
            f.score = combined[i];
            merged.push(f);
        }
    }
    // shard lists are each in order; a stable sort keeps shard order in ties
    merged.sort_by(|a, b| b.score.total_cmp(&a.score));
    merged
}

fn as_usize(v: Option<&Value>, default: usize) -> usize {
    v.and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .map(|n| n as usize)
        .unwrap_or(default)
}

/// Keys of a body that only shape the answer's page, dropped from the searches
/// that only count or only collect.
const PAGE_KEYS: &[&str] = &[
    "from",
    "size",
    "sort",
    "search_after",
    "collapse",
    "highlight",
    "explain",
    "rescore",
    "track_scores",
];

/// Run a hybrid query: the page, the total and the aggregations, as one
/// search's answer. With no scoring from a pipeline, the shards' lists come
/// back as the plugin leaves them, markers and all.
pub(crate) fn search(
    store: &Store,
    expr: &str,
    body: &Value,
    p: &Params,
    spec: &Hybrid,
    scoring: Option<&Scoring>,
) -> Result<Value, Response> {
    let mut targets = store.resolve(expr);
    targets.sort();
    let first_index = targets.first().cloned().unwrap_or_default();
    if p.contains_key("scroll") {
        return Err(shard_illegal(
            &first_index,
            "Scroll operation is not supported in hybrid query",
        ));
    }
    // term statistics gathered over every shard first would change the very
    // scores the pipeline is to scale, and the plugin does not take that
    if let Some(st) = p.get("search_type").filter(|t| t.as_str() != "query_then_fetch") {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("hybrid query does not support search_type [{st}]"),
        ));
    }
    let from = as_usize(body.get("from"), 0);
    let size = as_usize(body.get("size"), 10);
    if let Some(depth) = spec.depth {
        let window = store
            .get(&first_index)
            .and_then(|st| st.read().setting("max_result_window"))
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(10_000);
        let uuid =
            store.get(&first_index).and_then(|st| st.read().setting("uuid")).unwrap_or_default();
        let why = if depth == 0 {
            Some("pagination_depth must not be zero")
        } else if depth < 0 {
            Some("pagination_depth should be greater than 0")
        } else if depth > window {
            Some("pagination_depth should be less than or equal to index.max_result_window setting")
        } else {
            None
        };
        if let Some(why) = why {
            return Err(shard_failure(
                &first_index,
                json!({
                    "type": "query_shard_exception",
                    "reason": format!("failed to create query: {why}"),
                    "index": first_index,
                    "index_uuid": uuid,
                    "caused_by": {"type": "illegal_argument_exception", "reason": why},
                }),
            ));
        }
    } else if from > 0 {
        return Err(shard_illegal(
            &first_index,
            "pagination_depth param is missing in the search request",
        ));
    }
    let depth = spec.depth.map(|d| d as usize).unwrap_or(from + size);
    let union = as_bool(&spec.raw).map_err(|e| parsing(&e))?;

    // the total, the aggregations and the shard counts, over every document
    // any sub-query matches
    let mut main = body.clone();
    if let Some(o) = main.as_object_mut() {
        for k in PAGE_KEYS {
            o.remove(*k);
        }
    }
    main["query"] = union;
    main["size"] = json!(0);
    let mut plain = p.clone();
    for k in ["sort", "from", "size", "search_after"] {
        plain.remove(k);
    }
    let mut out = crate::search::run(store, expr, &main, &plain)?;

    // each shard's best documents for each sub-query
    let user_sort = body.get("sort").cloned();
    let sorted = user_sort.is_some();
    let mut shards: Vec<Vec<Vec<Found>>> = Vec::new();
    let mut raw_max: Option<f32> = None;
    for index in &targets {
        let mut lists = Vec::new();
        for q in &spec.queries {
            let mut filter = vec![json!({"term": {"_index": index}})];
            if let Some(f) = &spec.filter {
                filter.push(f.clone());
            }
            if let Some(pf) = body.get("post_filter") {
                filter.push(pf.clone());
            }
            let mut sub = json!({
                "query": {"bool": {"must": [q], "filter": filter}},
                "size": depth.max(1),
                "_source": false,
                "track_total_hits": false,
            });
            if let Some(rm) = body.get("runtime_mappings") {
                sub["runtime_mappings"] = rm.clone();
            }
            match &user_sort {
                Some(s) => {
                    sub["sort"] = s.clone();
                    sub["track_scores"] = json!(true);
                    if let Some(sa) = body.get("search_after") {
                        sub["search_after"] = sa.clone();
                    }
                }
                // documents that score the same come in the order they were
                // written, as Lucene's document numbers put them; this
                // engine's own numbering follows its segments instead
                None => sub["sort"] = json!([{"_score": "desc"}, {"_seq": "asc"}]),
            }
            let found = crate::search::run(store, expr, &sub, &plain)?;
            let mut list = Vec::new();
            for hit in &found.hits {
                let sort = hit.get("sort").and_then(|s| s.as_array());
                let score = hit
                    .get("_score")
                    .and_then(|s| s.as_f64())
                    .or_else(|| {
                        if sorted {
                            None
                        } else {
                            sort.and_then(|s| s.first()).and_then(|v| v.as_f64())
                        }
                    })
                    .unwrap_or(0.0) as f32;
                let doc = if sorted {
                    0
                } else {
                    sort.and_then(|s| s.get(1)).and_then(|v| v.as_u64()).unwrap_or(0)
                };
                raw_max = Some(raw_max.map_or(score, |m| m.max(score)));
                list.push(Found {
                    index: hit.get("_index").and_then(|v| v.as_str()).unwrap_or(index).to_string(),
                    id: hit.get("_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    doc,
                    score,
                });
            }
            if depth == 0 {
                list.clear();
            }
            lists.push(list);
        }
        shards.push(lists);
    }
    let subs = spec.queries.len();

    let Some(scoring) = scoring else {
        // no pipeline read the lists: each shard's comes back as it was sent,
        // its sub-query lists fenced by the markers
        let mut all: Vec<Found> = Vec::new();
        for lists in &shards {
            let Some(first) = lists.iter().flatten().next().cloned() else { continue };
            let marker = |score| Found { score, ..first.clone() };
            all.push(marker(MAGIC_START_STOP));
            for list in lists {
                all.push(marker(MAGIC_DELIMITER));
                all.extend(list.iter().cloned());
            }
            all.push(marker(MAGIC_START_STOP));
        }
        let page: Vec<Found> = all.into_iter().skip(from).take(size).collect();
        let hits = fetch(store, expr, body, &plain, spec, &page, false, 0, None)?;
        out.hits = hits;
        out.max_score = Some(raw_max.unwrap_or(0.0));
        return Ok(crate::search::envelope(out, body, p));
    };

    if !scoring.weights.is_empty() && scoring.weights.len() != subs {
        return Err(phase_failure(&format!(
            "number of weights [{}] must match number of sub-queries [{subs}] in hybrid query",
            scoring.weights.len()
        )));
    }
    if let Normalization::MinMax(bounds) = &scoring.normalization
        && !bounds.is_empty()
        && bounds.len() != subs
    {
        return Err(phase_failure(&format!(
            "expected bounds array to contain {subs} elements matching the number of sub-queries, \
             but found a mismatch"
        )));
    }
    if depth == 0 {
        out.hits = Vec::new();
        out.max_score = Some(raw_max.unwrap_or(0.0));
        return Ok(crate::search::envelope(out, body, p));
    }
    normalize(scoring, &mut shards, subs);
    let merged = combine_shards(scoring, &shards, subs);
    if from > merged.len() {
        return Err(phase_failure(
            "Reached end of search result, increase pagination_depth value to see more results",
        ));
    }
    let collapse_field =
        body.pointer("/collapse/field").and_then(|f| f.as_str()).map(str::to_string);
    let max_score = merged.first().map(|f| f.score).unwrap_or(0.0);
    if sorted {
        // the sort decides the order of every document any list holds
        let hits = fetch(store, expr, body, &plain, spec, &merged, true, from + size, None)?;
        out.hits = hits.into_iter().skip(from).take(size).collect();
        // one shard's sorted answer has no best score; the reference's merge
        // of several keeps the best combined one
        out.max_score = (out.shards > 1).then_some(max_score);
    } else if let Some(field) = collapse_field {
        let hits = fetch(store, expr, body, &plain, spec, &merged, false, 0, Some(&field))?;
        let mut seen: Vec<Value> = Vec::new();
        let mut kept = Vec::new();
        for hit in hits {
            let key = hit.pointer(&format!("/fields/{field}/0")).cloned().unwrap_or(Value::Null);
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            kept.push(hit);
        }
        out.hits = kept.into_iter().skip(from).take(size).collect();
        out.max_score = Some(max_score);
    } else {
        let page: Vec<Found> = merged.into_iter().skip(from).take(size).collect();
        out.hits = fetch(store, expr, body, &plain, spec, &page, false, 0, None)?;
        out.max_score = Some(max_score);
    }
    Ok(crate::search::envelope(out, body, p))
}

/// Read the documents a page names, with everything the request asked each
/// hit to carry. In the order given, scored as given -- or, for a sorted
/// search, in the order the sort puts them.
#[allow(clippy::too_many_arguments)]
fn fetch(
    store: &Store,
    expr: &str,
    body: &Value,
    p: &Params,
    spec: &Hybrid,
    page: &[Found],
    sorted: bool,
    want: usize,
    collapse: Option<&str>,
) -> Result<Vec<Value>, Response> {
    if page.is_empty() {
        return Ok(Vec::new());
    }
    let mut ids: Vec<&str> = page.iter().map(|f| f.id.as_str()).collect();
    ids.sort();
    ids.dedup();
    let indices = page.iter().map(|f| f.index.as_str()).collect::<std::collections::BTreeSet<_>>();
    let mut read = body.clone();
    if let Some(o) = read.as_object_mut() {
        for k in [
            "aggs",
            "aggregations",
            "post_filter",
            "from",
            "collapse",
            "highlight",
            "explain",
            "rescore",
            "suggest",
            "min_score",
            "search_after",
            "profile",
            "terminate_after",
            "stats",
            "indices_boost",
            "slice",
            "knn",
            "track_scores",
        ] {
            o.remove(k);
        }
        if !sorted {
            o.remove("sort");
        }
    }
    read["track_total_hits"] = json!(false);
    if let Some(field) = collapse {
        let mut dv =
            read.get("docvalue_fields").and_then(|d| d.as_array()).cloned().unwrap_or_default();
        dv.push(json!(field));
        read["docvalue_fields"] = Value::Array(dv);
    }
    let names = indices.iter().collect::<Vec<_>>();
    let by_ids = |read: &mut Value, ids: &[&str], size: usize| {
        let mut filter = json!({"bool": {"filter": [
            {"ids": {"values": ids}},
            {"terms": {"_index": names}},
        ]}});
        if let Some(n) = &spec.name {
            filter["bool"]["_name"] = json!(n);
        }
        read["query"] = filter;
        read["size"] = json!(size);
    };
    if sorted {
        // only the documents up to the end of the page are ever shown
        by_ids(&mut read, &ids, (ids.len() * names.len()).min(want));
        let found = crate::search::run(store, expr, &read, p)?;
        return Ok(found
            .hits
            .into_iter()
            .map(|mut h| {
                h["_score"] = Value::Null;
                h
            })
            .collect());
    }
    // a read asks for no more than a result window holds, so a long list is
    // read a window at a time
    let per_read = (10_000 / names.len().max(1)).max(1);
    let mut hits = Vec::new();
    for chunk in ids.chunks(per_read) {
        by_ids(&mut read, chunk, chunk.len() * names.len());
        hits.extend(crate::search::run(store, expr, &read, p)?.hits);
    }
    let mut by_key: HashMap<(String, String), Value> = HashMap::new();
    for hit in hits {
        let key = (
            hit.get("_index").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            hit.get("_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        );
        by_key.insert(key, hit);
    }
    Ok(page
        .iter()
        .filter_map(|f| {
            let mut hit = by_key.get(&(f.index.clone(), f.id.clone()))?.clone();
            hit["_score"] = json!(f.score);
            if hit.get("sort").is_some()
                && let Some(o) = hit.as_object_mut()
            {
                o.remove("sort");
            }
            Some(hit)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_order_follows_buckets_then_insertion() {
        // sixteen buckets hold up to twelve keys: 19 lands in bucket 3,
        // before 4 in bucket 4, and 3 shares 19's bucket after it
        let docs = [19u64, 4, 3, 29, 39];
        let order: Vec<u64> = java_hash_order(&docs).into_iter().map(|i| docs[i]).collect();
        assert_eq!(order, vec![19, 3, 4, 39, 29]);
    }

    #[test]
    fn min_max_scales_and_keeps_the_lowest_above_zero() {
        assert_eq!(min_max(2.0, 1.0, 3.0, None), 0.5);
        assert_eq!(min_max(1.0, 1.0, 3.0, None), MIN_SCORE);
        assert_eq!(min_max(1.0, 1.0, 1.0, None), SINGLE_RESULT_SCORE);
        let clip = LowerBound { mode: BoundMode::Clip, min_score: 1.5 };
        assert_eq!(min_max(1.2, 1.0, 3.0, Some(clip)), MIN_SCORE);
        assert_eq!(min_max(2.25, 1.0, 3.0, Some(clip)), 0.5);
    }

    #[test]
    fn combinations_skip_what_they_cannot_read() {
        assert_eq!(combine(Combination::Arithmetic, &[], &[1.0, 0.0]), 0.5);
        assert_eq!(combine(Combination::Harmonic, &[], &[0.5, 0.0]), 0.5);
        assert_eq!(combine(Combination::Geometric, &[0.3, 0.7], &[1.0, 0.0]), 1.0);
        assert_eq!(combine(Combination::Rrf, &[], &[0.5, 0.25]), 0.75);
    }
}
