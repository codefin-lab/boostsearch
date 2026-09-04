//! What happens to the candidates once every shard has answered.

use super::*;

/// `rescore` runs a second query over the top of the page and mixes its score
/// into the one already there.
pub(crate) fn apply_rescores(
    store: &Store,
    targets: &[String],
    cands: &mut [Cand],
    searchers: &Searchers,
    body: &Value,
    sort_keys: &[SortKey],
) -> std::result::Result<bool, Response> {
    // one rescore or several, written either way
    let rescores = match body.get("rescore") {
        Some(Value::Array(a)) => a.clone(),
        Some(one) => vec![one.clone()],
        None => vec![],
    };
    let mut rescored = false;
    for spec in &rescores {
        let window = spec.get("window_size").and_then(|v| v.as_u64()).unwrap_or(10).max(1) as usize;
        let inner = spec.get("query").cloned().unwrap_or(Value::Null);
        let Some(rq) = inner.get("rescore_query").cloned() else { continue };
        let qw = inner.get("query_weight").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
        let rw = inner.get("rescore_query_weight").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
        let mode = inner.get("score_mode").and_then(|v| v.as_str()).unwrap_or("total");
        cands.sort_by(|a, b| cmp_cands(a, b, sort_keys));
        let ids: Vec<String> = cands
            .iter()
            .take(window)
            .filter_map(|c| {
                let (_, searcher, st) = &searchers[c.shard];
                let g = st.read();
                source_of(searcher, &g, c.addr).map(|(id, _)| id)
            })
            .collect();
        if ids.is_empty() {
            continue;
        }
        let probe = json!({
            "query": {"bool": {"must": [rq], "filter": [{"terms": {"_id": ids.clone()}}]}},
            "size": ids.len(),
        });
        let Ok(answer) = run(store, &targets.join(","), &probe, &Params::new()) else { continue };
        let mut scored: std::collections::HashMap<String, f32> = std::collections::HashMap::new();
        for hit in &answer.hits {
            if let (Some(id), Some(score)) = (
                hit.get("_id").and_then(|v| v.as_str()),
                hit.get("_score").and_then(|v| v.as_f64()),
            ) {
                scored.insert(id.to_string(), score as f32);
            }
        }
        // the weight on the original query counts for every hit; only the
        // ones inside the window are also asked the second query
        for (at, c) in cands.iter_mut().enumerate() {
            let (_, searcher, st) = &searchers[c.shard];
            let g = st.read();
            let extra = if at < window {
                source_of(searcher, &g, c.addr).and_then(|(id, _)| scored.get(&id).copied())
            } else {
                None
            };
            match extra {
                Some(extra) => {
                    rescored = true;
                    c.score = match mode {
                        "multiply" => c.score * extra,
                        "max" => (c.score * qw).max(extra * rw),
                        "min" => (c.score * qw).min(extra * rw),
                        "avg" => (c.score * qw + extra * rw) / 2.0,
                        _ => c.score * qw + extra * rw,
                    };
                }
                None if mode != "multiply" => c.score *= qw,
                None => {}
            }
        }
    }
    Ok(rescored)
}

/// `indices_boost` weights whole indices against each other.
///
/// It is applied to the scores before they are ranked, and an alias may name
/// the index instead of the index naming itself.
pub(crate) fn apply_indices_boost(
    store: &Store,
    cands: &mut [Cand],
    searchers: &Searchers,
    boosts: &Value,
    p: &Params,
) -> std::result::Result<(), Response> {
    let mut pairs: Vec<(String, f32)> = Vec::new();
    let mut take = |o: &serde_json::Map<String, Value>| {
        for (k, v) in o {
            if let Some(b) = v.as_f64() {
                pairs.push((k.clone(), b as f32));
            }
        }
    };
    match boosts {
        Value::Object(o) => take(o),
        Value::Array(items) => {
            for item in items {
                if let Some(o) = item.as_object() {
                    take(o);
                }
            }
        }
        _ => {}
    }
    // a boost naming nothing is a request for an index that is not there,
    // unless the caller said to pass over what is missing
    let lenient = p.get("ignore_unavailable").map(|v| v != "false").unwrap_or(false);
    if !lenient {
        for (pat, _) in &pairs {
            // the cluster's indices and aliases, not this node's share: a
            // boost may name an index whose copies are on another node
            let known = pat.contains('*')
                || store.exists(pat)
                || store.get(pat).is_some()
                || !crate::api::cluster_resolve(store, pat).is_empty();
            if !known {
                return Err(err(
                    StatusCode::NOT_FOUND,
                    "index_not_found_exception",
                    format!("no such index [{pat}]"),
                ));
            }
        }
    }
    if !pairs.is_empty() {
        let names: Vec<String> = searchers.iter().map(|(n, _, _)| n.clone()).collect();
        let factor: Vec<f32> = names
            .iter()
            .map(|n| {
                pairs
                    .iter()
                    .find(|(pat, _)| {
                        pat == n
                            || crate::store::glob_match(pat, n)
                            || store.get(pat).map(|st| st.read().name == *n).unwrap_or(false)
                    })
                    .map(|(_, b)| *b)
                    .unwrap_or(1.0)
            })
            .collect();
        for c in cands.iter_mut() {
            if let Some(f) = factor.get(c.shard) {
                c.score *= f;
            }
        }
    }

    Ok(())
}
