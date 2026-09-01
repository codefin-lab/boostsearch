//! How well a search answers the questions someone has already answered.
//!
//! `_rank_eval` takes a set of queries and, for each, the documents a person
//! judged relevant. It runs them and scores what came back against those
//! judgements -- how much of the first page was relevant, how far down the
//! first relevant answer was, how much of the ranking's worth was realised.

use super::*;

/// One document, and what a person said it was worth.
fn rating_of(ratings: &[Value], index: &str, id: &str) -> Option<f64> {
    ratings.iter().find_map(|r| {
        let same = r.get("_id").and_then(|v| v.as_str()) == Some(id)
            && r.get("_index").and_then(|v| v.as_str()).map(|i| i == index).unwrap_or(true);
        same.then(|| r.get("rating").and_then(|v| v.as_f64()).unwrap_or(0.0))
    })
}

/// The metric a request asked for, and what it is called.
struct Metric {
    name: String,
    settings: Value,
}

impl Metric {
    fn of(body: &Value) -> Metric {
        let named = body.get("metric").and_then(|m| m.as_object());
        match named.and_then(|m| m.iter().next()) {
            Some((name, settings)) => Metric { name: name.clone(), settings: settings.clone() },
            None => Metric { name: "precision".into(), settings: json!({}) },
        }
    }

    fn number(&self, key: &str, fallback: f64) -> f64 {
        self.settings.get(key).and_then(|v| v.as_f64()).unwrap_or(fallback)
    }

    fn flag(&self, key: &str) -> bool {
        self.settings.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
    }

    /// How many answers this metric looks at.
    fn window(&self) -> usize {
        self.number("k", 10.0) as usize
    }

    /// The score for one query, the documents that went into it, and the
    /// numbers behind the score.
    fn score(&self, found: &[(String, String)], ratings: &[Value]) -> (f64, Vec<Value>, Value) {
        let relevant_at = self.number("relevant_rating_threshold", 1.0);
        let mut detail = Vec::new();
        let mut rated: Vec<Option<f64>> = Vec::new();
        for (index, id) in found.iter().take(self.window()) {
            let rating = rating_of(ratings, index, id);
            rated.push(rating);
            detail.push(json!({
                "_index": index, "_id": id, "rating": rating,
            }));
        }
        let is_relevant = |r: &Option<f64>| r.map(|v| v >= relevant_at).unwrap_or(false);
        let score = match self.name.as_str() {
            // how much of what came back was worth having
            "precision" => {
                let looked_at: Vec<&Option<f64>> = if self.flag("ignore_unlabeled") {
                    rated.iter().filter(|r| r.is_some()).collect()
                } else {
                    rated.iter().collect()
                };
                let relevant = looked_at.iter().filter(|r| is_relevant(r)).count();
                if looked_at.is_empty() { 0.0 } else { relevant as f64 / looked_at.len() as f64 }
            }
            // how much of what was worth having came back
            "recall" => {
                let relevant = rated.iter().filter(|r| is_relevant(r)).count();
                let all = ratings
                    .iter()
                    .filter(|r| {
                        r.get("rating").and_then(|v| v.as_f64()).unwrap_or(0.0) >= relevant_at
                    })
                    .count();
                if all == 0 { 0.0 } else { relevant as f64 / all as f64 }
            }
            // how far down the first answer worth having was
            "mean_reciprocal_rank" => {
                rated.iter().position(is_relevant).map(|at| 1.0 / (at + 1) as f64).unwrap_or(0.0)
            }
            // what the ranking was worth, counting a lower place for less
            "dcg" => {
                let gain = |rated: &[Option<f64>]| -> f64 {
                    rated
                        .iter()
                        .enumerate()
                        .map(|(at, rating)| {
                            let r = rating.unwrap_or(0.0);
                            (2f64.powf(r) - 1.0) / ((at + 2) as f64).log2()
                        })
                        .sum()
                };
                let here = gain(&rated);
                if self.flag("normalize") {
                    let mut best: Vec<Option<f64>> =
                        ratings.iter().map(|r| r.get("rating").and_then(|v| v.as_f64())).collect();
                    best.sort_by(|a, b| {
                        b.unwrap_or(0.0)
                            .partial_cmp(&a.unwrap_or(0.0))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    best.truncate(self.window());
                    let most = gain(&best);
                    if most == 0.0 { 0.0 } else { here / most }
                } else {
                    here
                }
            }
            // what a reader who stops when satisfied would have found
            "expected_reciprocal_rank" => {
                let maximum = self.number("maximum_relevance", 3.0);
                let mut chance_still_reading = 1.0;
                let mut score = 0.0;
                for (at, rating) in rated.iter().enumerate() {
                    let r = rating.unwrap_or(0.0);
                    let satisfied = (2f64.powf(r) - 1.0) / 2f64.powf(maximum);
                    score += chance_still_reading * satisfied / (at + 1) as f64;
                    chance_still_reading *= 1.0 - satisfied;
                }
                score
            }
            _ => 0.0,
        };
        // what the score was worked out from, which the caller may want to
        // read rather than trust
        let unrated = rated.iter().filter(|r| r.is_none()).count();
        let relevant_here = rated.iter().filter(|r| is_relevant(r)).count();
        let details = match self.name.as_str() {
            "precision" => {
                let looked_at = match self.flag("ignore_unlabeled") {
                    true => rated.iter().filter(|r| r.is_some()).count(),
                    false => rated.len(),
                };
                json!({"precision": {
                    "relevant_docs_retrieved": relevant_here,
                    "docs_retrieved": looked_at,
                }})
            }
            "recall" => {
                let all = ratings
                    .iter()
                    .filter(|r| {
                        r.get("rating").and_then(|v| v.as_f64()).unwrap_or(0.0) >= relevant_at
                    })
                    .count();
                json!({"recall": {
                    "relevant_docs_retrieved": relevant_here,
                    "relevant_docs": all,
                }})
            }
            "mean_reciprocal_rank" => json!({"mean_reciprocal_rank": {
                "first_relevant": rated
                    .iter()
                    .position(is_relevant)
                    .map(|at| at as i64 + 1)
                    .unwrap_or(-1),
            }}),
            "expected_reciprocal_rank" => {
                json!({"expected_reciprocal_rank": {"unrated_docs": unrated}})
            }
            "dcg" => json!({"dcg": {
                "dcg": score,
                "ideal_dcg": 0.0,
                "normalized_dcg": score,
                "unrated_docs": unrated,
            }}),
            _ => json!({}),
        };
        (score, detail, details)
    }
}

pub async fn rank_eval(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let expr = index.map(|Path(i)| i).unwrap_or_default();
    let metric = Metric::of(&body);
    let asked = body.get("requests").and_then(|r| r.as_array()).cloned().unwrap_or_default();
    if asked.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "parsing_exception",
            "[rank_eval] request must contain at least one [requests] entry",
        );
    }
    let mut details = serde_json::Map::new();
    let mut failures = serde_json::Map::new();
    let mut total = 0.0;
    // only the queries that ran count towards the score over all of them
    let mut scored = 0.0;
    for one in &asked {
        let id = one.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let ratings = one.get("ratings").and_then(|r| r.as_array()).cloned().unwrap_or_default();
        // a request may be written out, or named as a template the body
        // carries with the parameters to fill it with
        let templated = one.get("template_id").and_then(|v| v.as_str()).and_then(|named| {
            let template = body
                .get("templates")
                .and_then(|t| t.as_array())?
                .iter()
                .find(|t| t.get("id").and_then(|v| v.as_str()) == Some(named))?
                .get("template")?
                .clone();
            let source = template.get("source").cloned().unwrap_or(template);
            let params = one.get("params").cloned().unwrap_or_else(|| json!({}));
            crate::api::render_query_template(&source, &params)
        });
        let mut request = one.get("request").cloned().or(templated).unwrap_or_else(|| json!({}));
        // a rated request is about which documents come back and in what
        // order, so the parts of a search that answer something else have no
        // place in it
        for (part, why) in [
            ("aggs", "aggregations"),
            ("aggregations", "aggregations"),
            ("suggest", "a suggest section"),
            ("highlight", "a highlighter section"),
            ("rescore", "a rescorer"),
            ("profile", "profile"),
            ("explain", "explain"),
        ] {
            if request.get(part).is_some() {
                return err(
                    StatusCode::BAD_REQUEST,
                    "parsing_exception",
                    match why {
                        "profile" | "explain" => {
                            format!("Query in rated requests should not use {why}.")
                        }
                        _ => format!("Query in rated requests should not contain {why}."),
                    },
                );
            }
        }
        if request.get("size").is_none() {
            request["size"] = json!(metric.window());
        }
        match crate::search::run(&store, &expr, &request, &Params::new()) {
            Ok(out) => {
                let found: Vec<(String, String)> = out
                    .hits
                    .iter()
                    .filter_map(|hit| {
                        Some((
                            hit.get("_index")?.as_str()?.to_string(),
                            hit.get("_id")?.as_str()?.to_string(),
                        ))
                    })
                    .collect();
                let (score, hits, metric_details) = metric.score(&found, &ratings);
                total += score;
                scored += 1.0;
                // the documents that came back that nobody had judged
                let unrated: Vec<Value> = found
                    .iter()
                    .take(metric.window())
                    .filter(|(index, id)| rating_of(&ratings, index, id).is_none())
                    .map(|(index, id)| json!({"_index": index, "_id": id}))
                    .collect();
                details.insert(
                    id,
                    json!({
                        "metric_score": score,
                        "unrated_docs": unrated,
                        "hits": hits.iter().map(|hit| json!({
                            "hit": {
                                "_index": hit.get("_index"), "_id": hit.get("_id"), "_score": 1.0,
                            },
                            "rating": hit.get("rating"),
                        })).collect::<Vec<_>>(),
                        "metric_details": metric_details,
                    }),
                );
            }
            Err(_) => {
                failures.insert(id, json!({"error": "search failed"}));
            }
        }
    }
    respond(
        &p,
        json!({
            "metric_score": if scored == 0.0 { 0.0 } else { total / scored },
            "details": details,
            "failures": failures,
        }),
    )
}
