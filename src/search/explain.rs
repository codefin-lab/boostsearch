//! Where a score came from, told the way Lucene tells it.
//!
//! BoostCore can say how it scored a document -- the idf, the tf, and what
//! they were made of. What it says is re-worded here into the tree OpenSearch
//! writes, down to the `weight(field:term in doc) [PerFieldSimilarity]` head
//! that names the term the leaf was scored for.

use super::*;
use boostcore::Term;
use boostcore::collector::TopDocs;
use boostcore::query::{Query, TermQuery};
use boostcore::schema::IndexRecordOption;

/// The explanation of one document's score under a query.
pub(crate) fn explain_document(g: &IdxState, query: &Value, id: &str) -> Option<Value> {
    let searcher = g.reader.searcher();
    let probe = TermQuery::new(Term::from_field_text(g.fields.id, id), IndexRecordOption::Basic);
    let hits = searcher.search(&probe, &TopDocs::with_limit(1).order_by_score()).ok()?;
    let (_, addr) = hits.first()?;
    let ctx = crate::query::Ctx {
        fields: &g.fields,
        mapping: &g.mapping,
        analysis: &g.analysis,
        index: &g.index,
        max_terms_count: g.max_terms_count(),
        max_regex_length: g.max_regex_length(),
        allow_expensive: true,
        observed_kinds: &g.observed_kinds,
        kinds_complete: g.kinds_complete,
        stats: &g.stats,
    };
    let built = crate::query::build(&ctx, query).ok()?;
    let explanation = built.explain(&searcher, *addr).ok()?;
    let mut tree = serde_json::to_value(&explanation).ok()?;
    // the terms the query was built from, in the order the leaves are scored
    let mut leaves: Vec<String> = Vec::new();
    for (field, text, _) in crate::search::query_terms_by_field(Some(query)) {
        // a query string names its fields inside the text: `tag:a`
        let parts: Vec<(String, String)> = match field.as_str() {
            "*" => text
                .split_whitespace()
                .map(|word| match word.split_once(':') {
                    Some((f, v)) if !f.is_empty() => (f.to_string(), v.to_string()),
                    _ => (field.clone(), word.to_string()),
                })
                .collect(),
            _ => vec![(field.clone(), text.clone())],
        };
        for (field, text) in parts {
            let (_, _, view) = ctx.resolve(&field, true);
            for token in crate::query::analyze_with(&ctx, view, &field, &text, None) {
                leaves.push(format!("{field}:{token}"));
            }
        }
    }
    let mut next = leaves.into_iter();
    reword(&mut tree, &mut next, addr.doc_id);
    Some(collapsed(tree))
}

/// A sum of one thing is the thing: Lucene writes a single-clause bool as
/// the clause itself.
fn collapsed(mut node: Value) -> Value {
    let one = node
        .get("description")
        .and_then(|d| d.as_str())
        .map(|d| d == "sum of:")
        .unwrap_or(false)
        && node.get("details").and_then(|d| d.as_array()).map(|d| d.len() == 1).unwrap_or(false);
    if one
        && let Some(only) =
            node.get_mut("details").and_then(|d| d.as_array_mut()).and_then(|d| d.pop())
    {
        return collapsed(only);
    }
    if let Some(details) = node.get_mut("details").and_then(|d| d.as_array_mut()) {
        let inner: Vec<Value> = details.drain(..).map(collapsed).collect();
        *details = inner;
    }
    node
}

/// BoostCore's wording, rewritten into Lucene's.
fn reword(node: &mut Value, leaves: &mut impl Iterator<Item = String>, doc: u32) {
    // what BoostCore notes for itself about a leaf is not part of the answer
    if let Some(o) = node.as_object_mut() {
        o.remove("context");
    }
    let description = node.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string();
    let value = node.get("value").cloned().unwrap_or(json!(0.0));
    if let Some(details) = node.get_mut("details").and_then(|d| d.as_array_mut()) {
        for child in details.iter_mut() {
            reword(child, leaves, doc);
        }
    } else if let Some(o) = node.as_object_mut() {
        o.insert("details".into(), json!([]));
    }
    // a score is a float, and is written as short as a float is written
    if let Some(o) = node.as_object_mut()
        && let Some(v) = o.get("value").and_then(|v| v.as_f64())
        && let Ok(short) = format!("{}", v as f32).parse::<f64>()
    {
        o.insert("value".into(), json!(short));
    }
    let renamed = match description.as_str() {
        "BooleanClause. sum of ..." => Some("sum of:".to_string()),
        // a constant score names what it is the constant score of, and says
        // nothing more about it
        "Const" => {
            let named = node
                .pointer("/details/0/description")
                .and_then(|d| d.as_str())
                .and_then(|d| d.strip_prefix("weight("))
                .and_then(|d| d.split(" in ").next())
                .unwrap_or("*:*")
                .to_string();
            if let Some(o) = node.as_object_mut() {
                o.insert("details".into(), json!([]));
            }
            Some(format!("ConstantScore({named})"))
        }
        "TermQuery, product of..." => {
            // the leaf becomes two: the weight, and the score it is the
            // result of
            let term = leaves.next().unwrap_or_default();
            let freq =
                node.pointer("/details/1/details/0/value").and_then(|v| v.as_f64()).unwrap_or(1.0);
            let inner = json!({
                "value": value,
                "description": format!("score(freq={freq:.1}), computed as boost * idf * tf from:"),
                "details": node.get("details").cloned().unwrap_or(json!([])),
            });
            if let Some(o) = node.as_object_mut() {
                o.insert("details".into(), json!([inner]));
            }
            Some(format!("weight({term} in {doc}) [PerFieldSimilarity], result of:"))
        }
        "idf" => Some("idf, sum of:".to_string()),
        "idf, computed as log(1 + (N - n + 0.5) / (n + 0.5))" => {
            Some("idf, computed as log(1 + (N - n + 0.5) / (n + 0.5)) from:".to_string())
        }
        "n, number of docs containing this term" => {
            Some("n, number of documents containing term".to_string())
        }
        "N, total number of docs" => Some("N, total number of documents with field".to_string()),
        "freq / (freq + k1 * (1 - b + b * dl / avgdl))" => {
            Some("tf, computed as freq / (freq + k1 * (1 - b + b * dl / avgdl)) from:".to_string())
        }
        _ => None,
    };
    if let Some(named) = renamed
        && let Some(o) = node.as_object_mut()
    {
        o.insert("description".into(), json!(named));
    }
}
