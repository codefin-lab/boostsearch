//! `terms` over an analysed text field.
//!
//! A text field holds tokens, not values: OpenSearch buckets what the
//! analyser made of the text, which is why it asks for `fielddata` before it
//! will do this at all. The tokens are read from the term dictionary, and each
//! one is counted by asking the index how many documents the query and that
//! token have in common.

use super::*;

/// How many distinct tokens are considered when the query narrows the
/// documents. The tokens are taken in order of how many documents carry them,
/// so the ones a bucket could be made of come first.
const CANDIDATES: usize = 2_000;

/// Is this an analysed text field -- one whose aggregation buckets tokens
/// rather than the value as written?
pub(crate) fn analysed_text_field(store: &Store, targets: &[String], field: &str) -> bool {
    for name in targets {
        let Some(st) = store.get(name) else { continue };
        let g = st.read();
        // a sub-field addressed through its parent's untouched view is not
        // the analysed field, whatever its parent is
        if g.mapping.raw_view_parent(field).is_some() {
            return false;
        }
        if matches!(g.mapping.type_of(field), Some("text" | "match_only_text" | "annotated_text")) {
            return true;
        }
    }
    false
}

/// Every token the field holds, with how many documents hold each, added up
/// over the indices and their segments.
fn tokens_of(store: &Store, targets: &[String], field: &str) -> Vec<(String, u64)> {
    let mut counts: std::collections::HashMap<String, u64> = Default::default();
    for name in targets {
        let Some(st) = store.get(name) else { continue };
        let g = st.read();
        let searcher = g.reader.searcher();
        let dyn_field = g.fields.dynamic;
        let path = field.replace('.', "\u{1}");
        let mut start = boostcore::Term::from_field_json_path(dyn_field, &path, true);
        start.append_type_and_str("");
        let prefix = start.serialized_value_bytes().to_vec();
        for reader in searcher.segment_readers() {
            let Ok(inverted) = reader.inverted_index(dyn_field) else { continue };
            let Ok(mut stream) = inverted.terms().stream() else { continue };
            while let Some((bytes, info)) = stream.next() {
                if !bytes.starts_with(&prefix) {
                    continue;
                }
                let Ok(token) = std::str::from_utf8(&bytes[prefix.len()..]) else { continue };
                *counts.entry(token.to_string()).or_default() += info.doc_freq as u64;
            }
        }
    }
    let mut out: Vec<(String, u64)> = counts.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// One `terms` aggregation over an analysed text field.
pub(crate) fn run_text_terms_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
    weighted: bool,
) -> std::result::Result<Value, Response> {
    let spec = def.get("terms").cloned().unwrap_or(json!({}));
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    let size = spec.get("size").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
    let min_doc_count = spec.get("min_doc_count").and_then(|v| v.as_u64()).unwrap_or(1);
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let order = spec.get("order").cloned();

    let candidates = tokens_of(store, targets, &field);
    // with nothing narrowing the documents, the term dictionary has already
    // counted them; otherwise each token is asked about in turn
    let everything = main_query.is_none()
        || main_query.as_ref().map(|q| q.get("match_all").is_some()).unwrap_or(false);
    let mut counted: Vec<(String, u64)> = Vec::new();
    if everything {
        counted = candidates;
    } else {
        for (token, _) in candidates.into_iter().take(CANDIDATES) {
            let narrowed = json!({"bool": {"filter": [
                {"term": {field.clone(): token.clone()}},
                main_query.clone().unwrap_or_else(|| json!({"match_all": {}})),
            ]}});
            let (count, _) = filtered_count(store, targets, &narrowed, &None)?;
            if count > 0 {
                counted.push((token, count));
            }
        }
    }
    counted.retain(|(_, c)| *c >= min_doc_count);
    // most documents first, and between two of the same size the smaller key
    counted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let other: u64 = counted.iter().skip(size).map(|(_, c)| *c).sum();
    counted.truncate(size);

    let mut buckets: Vec<Value> = Vec::new();
    for (token, count) in counted {
        let mut b = json!({"key": token.clone(), "doc_count": count});
        if sub_aggs.is_some() {
            let mut filters = vec![json!({"term": {field.clone(): token}})];
            if let Some(q) = main_query.as_ref() {
                filters.push(q.clone());
            }
            let narrowed = json!({"bool": {"filter": filters}});
            let (_, sub) = count_with_sub_aggs(store, targets, &narrowed, &sub_aggs, weighted)?;
            if let Some(o) = sub.as_ref().and_then(|s| s.as_object()) {
                for (k, v) in o {
                    b[k.clone()] = v.clone();
                }
            }
        }
        buckets.push(b);
    }
    // an order naming a sub-aggregation is applied once those have answered
    if let Some((key, desc)) = order
        .as_ref()
        .and_then(|o| o.as_object())
        .and_then(|o| o.iter().next())
        .map(|(k, v)| (k.clone(), v.as_str() == Some("desc")))
        && key != "_count"
    {
        buckets.sort_by(|a, b| {
            let ord = compare_bucket_by(a, b, &key);
            if desc { ord.reverse() } else { ord }
        });
    } else if order.as_ref().and_then(|o| o.get("_count")).and_then(|v| v.as_str()) == Some("asc") {
        buckets.reverse();
    }
    Ok(json!({
        "buckets": buckets,
        "sum_other_doc_count": other,
        "doc_count_error_upper_bound": 0,
    }))
}
