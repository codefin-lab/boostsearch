//! Searching a rollup index as though it were the raw data.
//!
//! A search of an index a rollup job wrote is rewritten before it runs: a
//! terms, histogram or date histogram aggregation over a dimension is asked of
//! the field the dimension's values were written under, a sum, minimum or
//! maximum of the field the job kept it in, and an average or a value count is
//! put back together from the sums and counts the job kept. The documents are
//! counted by the `_doc_count` each carries, so a bucket counts the raw
//! documents it stands for. What cannot be answered from the rollup documents
//! is refused, in the plugin's words.

use serde_json::{Value, json};

use crate::store::Store;

/// What a search of rollup indices comes to.
pub enum Intercepted {
    /// run this body instead, and put its answer back together with `finish`
    Rewritten { body: Value, plan: Value },
    /// some of the indices are rollups and some are not: the others are
    /// searched on their own and each rollup index answers as a failed shard
    Mixed { others: Vec<String>, failures: Vec<Value> },
    /// the search cannot be answered; the reason, and the indices it names
    Refused { reason: String, indices: Vec<String> },
}

/// The pipeline aggregations, which name no field and are left as they are.
const PIPELINES: &[&str] = &[
    "bucket_sort",
    "bucket_selector",
    "bucket_script",
    "derivative",
    "cumulative_sum",
    "moving_avg",
    "moving_fn",
    "serial_diff",
    "avg_bucket",
    "sum_bucket",
    "min_bucket",
    "max_bucket",
    "stats_bucket",
    "extended_stats_bucket",
    "percentiles_bucket",
];

const METRIC_AGGS: &[&str] = &["sum", "avg", "max", "min", "value_count", "cardinality"];
const DIMENSION_AGGS: &[&str] = &["terms", "date_histogram", "histogram"];

/// Whether searching rollup indices is rewritten at all.
fn search_enabled(store: &Store) -> bool {
    store
        .cluster_setting("plugins.rollup.search.enabled")
        .and_then(|v| v.as_bool().or_else(|| v.as_str().map(|s| s != "false")))
        .unwrap_or(true)
}

/// Look at a search before it runs. `None` where no rollup index is involved.
pub fn intercept(
    store: &Store,
    targets: &[String],
    body: &Value,
    params: &crate::api::Params,
) -> Option<Intercepted> {
    if !search_enabled(store) || targets.is_empty() {
        return None;
    }
    let rollups: Vec<String> =
        targets.iter().filter(|t| super::rollup::is_rollup_index(store, t)).cloned().collect();
    if rollups.is_empty() {
        return None;
    }
    if rollups.len() != targets.len() {
        let failures = rollups
            .iter()
            .map(|index| {
                json!({"shard": 0, "index": index, "node": crate::cluster::identity().name.clone(),
                    "reason": {"type": "illegal_argument_exception",
                        "reason": "Not all indices have rollup job"}})
            })
            .collect();
        let others = targets.iter().filter(|t| !rollups.contains(t)).cloned().collect();
        return Some(Intercepted::Mixed { others, failures });
    }
    let refuse = |reason: String| Some(Intercepted::Refused { reason, indices: rollups.clone() });
    let size = params
        .get("size")
        .and_then(|s| s.parse::<i64>().ok())
        .or_else(|| body.get("size").and_then(|v| v.as_i64()))
        .unwrap_or(-1);
    if size != 0 {
        return refuse(format!(
            "Rollup search must have size explicitly set to 0, but found {size}"
        ));
    }
    // what the search asks of fields: the query first, then the aggregations.
    // A term names a terms dimension; a range may be over any dimension, so
    // only its field is known.
    let mut known: Vec<(bool, String, String)> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    if let Some(q) = body.get("query")
        && let Err(reason) = query_fields(q, &mut known, &mut unknown)
    {
        return refuse(reason);
    }
    let aggs = body.get("aggs").or_else(|| body.get("aggregations")).cloned();
    if let Some(aggs) = &aggs
        && let Err(reason) = aggregation_fields(aggs, &mut known)
    {
        return refuse(reason);
    }
    // the job, per index, that has every field the search asks for
    let mut chosen: Vec<Value> = Vec::new();
    for index in &rollups {
        let jobs = super::rollup::jobs_of_index(store, index);
        if jobs.is_empty() {
            return refuse("No rollup job associated with target_index".into());
        }
        let fits: Vec<&Value> = jobs
            .iter()
            .filter(|job| {
                let have = field_mappings(job);
                known.iter().all(|k| have.contains(k))
                    && unknown.iter().all(|u| have.iter().any(|(_, f, _)| f == u))
            })
            .collect();
        if fits.is_empty() {
            let have: Vec<(bool, String, String)> = jobs.iter().flat_map(field_mappings).collect();
            let mut issues: Vec<String> = Vec::new();
            let mut note = |s: String| {
                if !issues.contains(&s) {
                    issues.push(s);
                }
            };
            for (dimension, field, kind) in &known {
                if !have.iter().any(|(_, f, _)| f == field) {
                    note(format!("missing field {field}"));
                } else if !have.contains(&(*dimension, field.clone(), kind.clone())) {
                    note(if *dimension {
                        format!("missing {kind} grouping on {field}")
                    } else {
                        format!("missing {kind} aggregation on {field}")
                    });
                }
            }
            for field in &unknown {
                if !have.iter().any(|(_, f, _)| f == field) {
                    note(format!("missing field {field}"));
                }
            }
            return refuse(format!(
                "Could not find a rollup job that can answer this query because [{}]",
                issues.join(", ")
            ));
        }
        // of several that fit, the one with the widest window answers
        let best = fits
            .iter()
            .max_by_key(|job| estimated_interval(job))
            .map(|j| (*j).clone())
            .unwrap_or(Value::Null);
        chosen.push(best);
    }
    let job = chosen[0].clone();
    let ids: Vec<Value> = chosen.iter().map(|j| j["rollup_id"].clone()).collect();
    let mut plan = json!({});
    let mut rewritten = body.clone();
    let query =
        body.get("query").map(|q| rewrite_query(&job, q)).unwrap_or(json!({"match_all": {}}));
    rewritten["query"] = json!({"bool": {"must": [query],
        "should": [{"terms": {"rollup._id": ids}}], "minimum_should_match": 1}});
    if let Some(aggs) = &aggs {
        let mut plan_aggs = serde_json::Map::new();
        let done = rewrite_aggs(&job, aggs, &mut plan_aggs);
        if let Some(o) = rewritten.as_object_mut() {
            o.remove("aggregations");
            o.insert("aggs".into(), done);
        }
        plan = Value::Object(plan_aggs);
    }
    let asked_query = body.get("query").map(|q| q.get("match_all").is_none()).unwrap_or(false);
    let plan = json!({"aggs": plan, "asked_query": asked_query});
    Some(Intercepted::Rewritten { body: rewritten, plan })
}

/// The fields a job has, as the plugin lists them: each dimension by the kind
/// of grouping it is, and each metric by the kind of aggregation.
fn field_mappings(job: &Value) -> Vec<(bool, String, String)> {
    let mut out = Vec::new();
    for d in job["dimensions"].as_array().into_iter().flatten() {
        if let Some((kind, source, _)) = super::rollup::dimension_field(d) {
            out.push((true, source, kind));
        }
    }
    for m in job["metrics"].as_array().into_iter().flatten() {
        let source = m["source_field"].as_str().unwrap_or_default().to_string();
        for one in m["metrics"].as_array().into_iter().flatten() {
            if let Some(kind) = one.as_object().and_then(|o| o.keys().next()) {
                out.push((false, source.clone(), kind.clone()));
            }
        }
    }
    out
}

fn estimated_interval(job: &Value) -> i64 {
    let hist = &job["dimensions"][0]["date_histogram"];
    hist["fixed_interval"]
        .as_str()
        .or_else(|| hist["calendar_interval"].as_str())
        .and_then(|s| {
            crate::search::CalendarUnit::parse(s)
                .map(|_| match s {
                    "month" | "1M" => 30 * 86_400_000,
                    "quarter" | "1q" => 90 * 86_400_000,
                    "year" | "1y" => 365 * 86_400_000,
                    "week" | "1w" => 7 * 86_400_000,
                    "day" | "1d" => 86_400_000,
                    "hour" | "1h" => 3_600_000,
                    "minute" | "1m" => 60_000,
                    _ => 1_000,
                })
                .or_else(|| {
                    crate::search::aggs::parse_offset(s).map(|d| d.whole_milliseconds() as i64)
                })
        })
        .unwrap_or(0)
}

fn kind_and_body(def: &Value) -> Option<(String, Value)> {
    def.as_object()?
        .iter()
        .find(|(k, _)| !matches!(k.as_str(), "aggs" | "aggregations" | "meta"))
        .map(|(k, v)| (k.clone(), v.clone()))
}

fn sub_aggs(def: &Value) -> Option<&Value> {
    def.get("aggs").or_else(|| def.get("aggregations"))
}

fn aggregation_fields(aggs: &Value, out: &mut Vec<(bool, String, String)>) -> Result<(), String> {
    for (_, def) in aggs.as_object().into_iter().flatten() {
        let Some((kind, body)) = kind_and_body(def) else { continue };
        if PIPELINES.contains(&kind.as_str()) {
            continue;
        }
        let field = body.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
        if DIMENSION_AGGS.contains(&kind.as_str()) {
            out.push((true, field, kind.clone()));
        } else if METRIC_AGGS.contains(&kind.as_str()) {
            out.push((false, field, kind.clone()));
        } else {
            return Err(format!("The {kind} aggregation is not currently supported in rollups"));
        }
        if let Some(sub) = sub_aggs(def) {
            aggregation_fields(sub, out)?;
        }
    }
    Ok(())
}

fn query_fields(
    q: &Value,
    known: &mut Vec<(bool, String, String)>,
    out: &mut Vec<String>,
) -> Result<(), String> {
    let Some((kind, body)) = q.as_object().and_then(|o| o.iter().next()) else { return Ok(()) };
    let first_field = |body: &Value| {
        body.as_object()
            .and_then(|o| o.keys().find(|k| !matches!(k.as_str(), "boost" | "_name")).cloned())
    };
    match kind.as_str() {
        "term" | "terms" => {
            if let Some(f) = first_field(body) {
                known.push((true, f, "terms".into()));
            }
        }
        "range" => {
            if let Some(f) = first_field(body) {
                out.push(f);
            }
        }
        "match_all" => {}
        "bool" => {
            for clause in ["must", "must_not", "should", "filter"] {
                match body.get(clause) {
                    Some(Value::Array(list)) => {
                        for one in list {
                            query_fields(one, known, out)?;
                        }
                    }
                    Some(one @ Value::Object(_)) => query_fields(one, known, out)?,
                    _ => {}
                }
            }
        }
        "boosting" => {
            for part in ["positive", "negative"] {
                if let Some(inner) = body.get(part) {
                    query_fields(inner, known, out)?;
                }
            }
        }
        "constant_score" => {
            if let Some(inner) = body.get("filter") {
                query_fields(inner, known, out)?;
            }
        }
        "dis_max" => {
            for inner in body.get("queries").and_then(|v| v.as_array()).into_iter().flatten() {
                query_fields(inner, known, out)?;
            }
        }
        "match_phrase" => {
            let Some(f) = first_field(body) else { return Ok(()) };
            let opts = &body[f.as_str()];
            let plain = !opts.is_object()
                || (opts.get("analyzer").is_none()
                    && opts.get("slop").and_then(|v| v.as_i64()).unwrap_or(0) == 0
                    && opts
                        .get("zero_terms_query")
                        .and_then(|v| v.as_str())
                        .map(|z| z.eq_ignore_ascii_case("none"))
                        .unwrap_or(true));
            if !plain {
                return Err(
                    "The match_phrase query is currently not supported with analyzer/slop/zero_terms_query in rollups"
                        .into(),
                );
            }
            known.push((true, f, "terms".into()));
        }
        "query_string" => {
            let text = body.get("query").and_then(|v| v.as_str()).unwrap_or("");
            for token in text.split(|c: char| c.is_whitespace() || c == '(' || c == ')') {
                if let Some((field, _)) = token.split_once(':')
                    && !field.is_empty()
                    && field != "_exists_"
                {
                    out.push(field.trim_start_matches(['+', '-']).to_string());
                }
            }
            if let Some(f) = body.get("default_field").and_then(|v| v.as_str())
                && !text.contains(':')
            {
                out.push(f.to_string());
            }
        }
        other => return Err(format!("The {other} query is currently not supported in rollups")),
    }
    Ok(())
}

/// The name a source field's dimension values were written under.
fn dimension_target(job: &Value, field: &str) -> String {
    for d in job["dimensions"].as_array().into_iter().flatten() {
        if let Some((_, source, name)) = super::rollup::dimension_field(d)
            && source == field
        {
            return name;
        }
    }
    format!("{field}.terms")
}

fn metric_target(job: &Value, field: &str, kind: &str) -> String {
    for m in job["metrics"].as_array().into_iter().flatten() {
        if m["source_field"].as_str() == Some(field) {
            let target = m["target_field"].as_str().unwrap_or(field);
            return format!("{target}.{kind}");
        }
    }
    format!("{field}.{kind}")
}

fn rewrite_query(job: &Value, q: &Value) -> Value {
    let Some((kind, body)) = q.as_object().and_then(|o| o.iter().next()) else { return q.clone() };
    let rename_first = |body: &Value| -> Value {
        let mut out = serde_json::Map::new();
        for (k, v) in body.as_object().into_iter().flatten() {
            if matches!(k.as_str(), "boost" | "_name") {
                out.insert(k.clone(), v.clone());
            } else {
                out.insert(dimension_target(job, k), v.clone());
            }
        }
        Value::Object(out)
    };
    match kind.as_str() {
        "term" | "terms" | "range" | "match_phrase" => json!({kind: rename_first(body)}),
        "bool" => {
            let mut out = body.clone();
            for clause in ["must", "must_not", "should", "filter"] {
                match body.get(clause) {
                    Some(Value::Array(list)) => {
                        out[clause] =
                            Value::Array(list.iter().map(|q| rewrite_query(job, q)).collect());
                    }
                    Some(one @ Value::Object(_)) => out[clause] = rewrite_query(job, one),
                    _ => {}
                }
            }
            json!({"bool": out})
        }
        "boosting" => {
            let mut out = body.clone();
            for part in ["positive", "negative"] {
                if let Some(inner) = body.get(part) {
                    out[part] = rewrite_query(job, inner);
                }
            }
            json!({"boosting": out})
        }
        "constant_score" => {
            let mut out = body.clone();
            if let Some(inner) = body.get("filter") {
                out["filter"] = rewrite_query(job, inner);
            }
            json!({"constant_score": out})
        }
        "dis_max" => {
            let mut out = body.clone();
            if let Some(list) = body.get("queries").and_then(|v| v.as_array()) {
                out["queries"] = Value::Array(list.iter().map(|q| rewrite_query(job, q)).collect());
            }
            json!({"dis_max": out})
        }
        "query_string" => {
            let mut out = body.clone();
            if let Some(text) = body.get("query").and_then(|v| v.as_str()) {
                let mut rewritten = String::new();
                for (i, token) in text.split(' ').enumerate() {
                    if i > 0 {
                        rewritten.push(' ');
                    }
                    match token.split_once(':') {
                        Some((field, rest)) if !field.is_empty() && field != "_exists_" => {
                            let bare = field.trim_start_matches(['+', '-', '(']);
                            let prefix = &field[..field.len() - bare.len()];
                            rewritten.push_str(&format!(
                                "{prefix}{}:{rest}",
                                dimension_target(job, bare)
                            ));
                        }
                        _ => rewritten.push_str(token),
                    }
                }
                out["query"] = json!(rewritten);
            }
            json!({"query_string": out})
        }
        _ => q.clone(),
    }
}

/// Rewrite a level of aggregations, noting in `plan` which of them have to be
/// put back together once the answer is in.
fn rewrite_aggs(job: &Value, aggs: &Value, plan: &mut serde_json::Map<String, Value>) -> Value {
    let mut out = serde_json::Map::new();
    for (name, def) in aggs.as_object().into_iter().flatten() {
        let Some((kind, body)) = kind_and_body(def) else {
            out.insert(name.clone(), def.clone());
            continue;
        };
        let field = body.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
        let mut sub_plan = serde_json::Map::new();
        let sub = sub_aggs(def).map(|s| rewrite_aggs(job, s, &mut sub_plan));
        let mut one = match kind.as_str() {
            "terms" | "date_histogram" | "histogram" => {
                let mut b = body.clone();
                if let Some(o) = b.as_object_mut() {
                    // what the plugin carries over into the rewritten grouping
                    // leaves these behind: a zone and a format are the job's,
                    // and a missing value has no document to stand in for
                    for dropped in ["missing", "script", "value_type", "time_zone", "format"] {
                        o.remove(dropped);
                    }
                    o.insert("field".into(), json!(dimension_target(job, &field)));
                    // the buckets are the job's days, so they are days in the
                    // job's zone
                    if kind == "date_histogram"
                        && let Some(zone) = job["dimensions"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|d| d.get("date_histogram"))
                            .find(|d| d["source_field"].as_str() == Some(field.as_str()))
                            .and_then(|d| d["timezone"].as_str())
                            .filter(|z| *z != "UTC")
                    {
                        o.insert("time_zone".into(), json!(zone));
                    }
                }
                json!({kind.clone(): b})
            }
            "sum" | "min" | "max" => {
                json!({kind.clone(): {"field": metric_target(job, &field, &kind)}})
            }
            "value_count" => {
                plan.insert(name.clone(), json!({"kind": "value_count"}));
                json!({"sum": {"field": metric_target(job, &field, "value_count")}})
            }
            "avg" => {
                let helper = format!("__rollup_count_{name}");
                plan.insert(name.clone(), json!({"kind": "avg", "helper": helper}));
                out.insert(
                    helper,
                    json!({"sum": {"field": format!("{}.value_count", metric_target(job, &field, "avg"))}}),
                );
                json!({"sum": {"field": format!("{}.sum", metric_target(job, &field, "avg"))}})
            }
            _ => def.clone(),
        };
        if let Some(sub) = sub
            && sub.as_object().map(|o| !o.is_empty()).unwrap_or(false)
        {
            one["aggs"] = sub;
        }
        if !sub_plan.is_empty() {
            let entry = plan.entry(name.clone()).or_insert_with(|| json!({}));
            entry["sub"] = Value::Object(sub_plan);
        }
        out.insert(name.clone(), one);
    }
    Value::Object(out)
}

/// Put an answer to a rewritten search back together: an average from its
/// sum and count, a value count as a whole number.
pub fn finish(answer: &mut Value, plan: &Value) {
    if let Some(aggs) = answer.get_mut("aggregations") {
        finish_level(aggs, &plan["aggs"]);
    }
    // the reference counts a rollup search over every document without
    // looking at them, and says so; one with a query of its own it answers in
    // full
    if plan.get("asked_query").and_then(|v| v.as_bool()) == Some(true)
        && let Some(o) = answer.as_object_mut()
    {
        o.remove("terminated_early");
    }
}

fn finish_level(level: &mut Value, plan: &Value) {
    let Some(entries) = plan.as_object() else { return };
    for (name, step) in entries {
        match step.get("kind").and_then(|k| k.as_str()) {
            Some("avg") => {
                let helper = step["helper"].as_str().unwrap_or_default().to_string();
                let count = level
                    .as_object_mut()
                    .and_then(|o| o.remove(&helper))
                    .and_then(|h| h.get("value").and_then(|v| v.as_f64()))
                    .unwrap_or(0.0);
                if let Some(slot) = level.get_mut(name) {
                    let sum = slot.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    *slot = json!({"value": super::jobs::double_value(sum / count)});
                }
            }
            Some("value_count") => {
                if let Some(slot) = level.get_mut(name) {
                    let n = slot.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    *slot = json!({"value": n as i64});
                }
            }
            _ => {}
        }
        if let Some(sub) = step.get("sub")
            && let Some(slot) = level.get_mut(name)
        {
            match slot.get_mut("buckets") {
                Some(Value::Array(list)) => list.iter_mut().for_each(|b| finish_level(b, sub)),
                Some(Value::Object(keyed)) => keyed.values_mut().for_each(|b| finish_level(b, sub)),
                _ => {}
            }
        }
    }
}

/// The refusal a search of rollup indices is answered with: every shard
/// failed, and each for the same reason.
pub fn refusal(reason: &str, indices: &[String]) -> axum::response::Response {
    let detail = json!({"type": "illegal_argument_exception", "reason": reason});
    let node = crate::cluster::identity().name.clone();
    let shards: Vec<Value> = indices
        .iter()
        .map(|i| json!({"shard": 0, "index": i, "node": node, "reason": detail}))
        .collect();
    let body = json!({
        "error": {
            "root_cause": [detail],
            "type": "search_phase_execution_exception",
            "reason": "all shards failed",
            "phase": "query",
            "grouped": true,
            "failed_shards": shards,
            "caused_by": {"type": "illegal_argument_exception", "reason": reason,
                "caused_by": detail},
        },
        "status": 400,
    });
    axum::response::IntoResponse::into_response((
        axum::http::StatusCode::BAD_REQUEST,
        axum::Json(body),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> Value {
        json!({"rollup_id": "r", "dimensions": [
            {"date_histogram": {"source_field": "sold_at", "target_field": "sold_at",
                "fixed_interval": "1d", "timezone": "UTC"}},
            {"terms": {"source_field": "region", "target_field": "region"}}],
            "metrics": [{"source_field": "revenue", "metrics": [{"sum": {}}, {"avg": {}},
                {"value_count": {}}]}]})
    }

    #[test]
    fn an_average_is_put_back_together() {
        let aggs = json!({"r": {"terms": {"field": "region"}, "aggs": {"a": {"avg": {"field": "revenue"}},
            "c": {"value_count": {"field": "revenue"}}}}});
        let mut plan = serde_json::Map::new();
        let rewritten = rewrite_aggs(&job(), &aggs, &mut plan);
        assert_eq!(rewritten["r"]["terms"]["field"], json!("region.terms"));
        assert_eq!(rewritten["r"]["aggs"]["a"], json!({"sum": {"field": "revenue.avg.sum"}}));
        let mut answer = json!({"aggregations": {"r": {"buckets": [{"key": "x", "doc_count": 3,
            "a": {"value": 9.0}, "__rollup_count_a": {"value": 3.0}, "c": {"value": 3.0}}]}}});
        finish(&mut answer, &json!({"aggs": Value::Object(plan)}));
        let bucket = &answer["aggregations"]["r"]["buckets"][0];
        assert_eq!(bucket["a"], json!({"value": 3.0}));
        assert_eq!(bucket["c"], json!({"value": 3}));
        assert!(bucket.get("__rollup_count_a").is_none());
    }

    #[test]
    fn what_cannot_be_answered_is_named() {
        let mut known = Vec::new();
        assert_eq!(
            aggregation_fields(&json!({"x": {"top_hits": {}}}), &mut known).unwrap_err(),
            "The top_hits aggregation is not currently supported in rollups"
        );
        let mut unknown = Vec::new();
        assert_eq!(
            query_fields(&json!({"prefix": {"region": "no"}}), &mut known, &mut unknown)
                .unwrap_err(),
            "The prefix query is currently not supported in rollups"
        );
    }
}
