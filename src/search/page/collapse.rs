//! One hit per distinct value of a field, and the group behind it.

use super::*;

/// The documents a collapsed hit was chosen from: the same query, narrowed to
/// the one value, asked again with whatever the `inner_hits` clause says.
pub(crate) fn collapsed_group(
    store: &Store,
    targets: &[String],
    query: Option<&Value>,
    field: &str,
    value: &Value,
    inner: &Value,
    p: &Params,
) -> Option<Value> {
    // the value the group stands for narrows it; the query that found the
    // group still scores it, which is what decides the order inside
    let mut group = json!({"bool": {"filter": [{"term": {field: value.clone()}}]}});
    if let Some(q) = query {
        group["bool"]["must"] = json!([q.clone()]);
    }
    let mut body = json!({"query": group});
    for key in [
        "size",
        "from",
        "sort",
        "_source",
        "version",
        "seq_no_primary_term",
        "docvalue_fields",
        "stored_fields",
        "highlight",
        "explain",
        "fields",
        // the group may be collapsed again, on a field of its own
        "collapse",
    ] {
        if let Some(v) = inner.get(key) {
            body[key] = v.clone();
        }
    }
    let out = run(store, &targets.join(","), &body, &Params::new()).ok()?;
    let total = if p.get("rest_total_hits_as_int").map(|v| v == "true").unwrap_or(false) {
        json!(out.total)
    } else {
        json!({"value": out.total, "relation": "eq"})
    };
    let max_score = out.max_score.map(|s| json!(s)).unwrap_or(Value::Null);
    Some(json!({"hits": {"total": total, "max_score": max_score, "hits": out.hits}}))
}
