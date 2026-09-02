//! Reshaping an aggregation request into the one BoostCore is given, and
//! the answer back into the one OpenSearch would have sent.

use super::*;

/// Dates in aggregation parameters may be date-only; BoostCore needs RFC3339.
pub(crate) fn normalize_agg_dates(node: &mut Value) {
    match node {
        Value::Object(o) => {
            for (k, v) in o.iter_mut() {
                if matches!(k.as_str(), "min" | "max" | "from" | "to")
                    && let Value::String(s) = v
                    && s.len() == 10
                    && s.matches('-').count() == 2
                {
                    *v = json!(format!("{s}T00:00:00Z"));
                    continue;
                }
                normalize_agg_dates(v);
            }
        }
        Value::Array(a) => {
            for v in a {
                normalize_agg_dates(v);
            }
        }
        _ => {}
    }
}

/// Render a small subset of the query DSL as a BoostCore query string, so a
/// `filter` aggregation nested inside another bucket can still run.
pub(crate) fn as_boostcore_query_string(q: &Value, ctx: &Ctx) -> Option<String> {
    let o = q.as_object()?;
    let (kind, body) = o.iter().next()?;
    match kind.as_str() {
        "match_all" => Some("*".to_string()),
        "term" | "match" | "match_phrase" => {
            let (field, spec) = body.as_object()?.iter().next()?;
            let value = spec.get("value").or_else(|| spec.get("query")).unwrap_or(spec);
            let col = ctx.column_name(field, kind != "term");
            let text = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            Some(format!("{col}:\"{text}\""))
        }
        "range" => {
            let (field, spec) = body.as_object()?.iter().next()?;
            let col = ctx.column_name(field, false);
            let lo = spec.get("gte").or_else(|| spec.get("gt"));
            let hi = spec.get("lte").or_else(|| spec.get("lt"));
            let fmt = |v: Option<&Value>| match v {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => "*".to_string(),
            };
            Some(format!("{col}:[{} TO {}]", fmt(lo), fmt(hi)))
        }
        "bool" => {
            let mut parts = Vec::new();
            for key in ["must", "filter"] {
                if let Some(list) = body.get(key) {
                    let items: Vec<Value> = match list {
                        Value::Array(a) => a.clone(),
                        other => vec![other.clone()],
                    };
                    for it in items {
                        parts.push(format!("+({})", as_boostcore_query_string(&it, ctx)?));
                    }
                }
            }
            if let Some(list) = body.get("must_not") {
                let items: Vec<Value> = match list {
                    Value::Array(a) => a.clone(),
                    other => vec![other.clone()],
                };
                for it in items {
                    parts.push(format!("-({})", as_boostcore_query_string(&it, ctx)?));
                }
            }
            if parts.is_empty() { None } else { Some(parts.join(" ")) }
        }
        _ => None,
    }
}

/// Nested `filter` aggregations become BoostCore's own filter, which speaks
/// query strings. Top-level ones are handled by running a separate search.
pub(crate) fn lower_nested_filters(node: &mut Value, ctx: &Ctx) {
    let Some(o) = node.as_object_mut() else { return };
    for (_, def) in o.iter_mut() {
        if let Some(sub) = def.get_mut("aggs")
            && let Some(subo) = sub.as_object_mut()
        {
            for (_, sdef) in subo.iter_mut() {
                if let Some(f) = sdef.get("filter").cloned()
                    && !f.is_string()
                    && let Some(qs) = as_boostcore_query_string(&f, ctx)
                    && let Some(o) = sdef.as_object_mut()
                {
                    o.insert("filter".into(), json!(qs));
                }
            }
            lower_nested_filters(sub, ctx);
        }
    }
}

/// A terms aggregation may ask for one slice of the term space rather than the
/// whole of it. BoostCore has no such notion, so the slice is taken here: the
/// request goes down without the `include`, asking for enough terms that the
/// wanted partition is whole, and the rest are dropped from the answer.
pub(crate) fn extract_partitions(node: &mut Value) -> Vec<(String, i64, i64, usize)> {
    let mut out = Vec::new();
    let Some(o) = node.as_object_mut() else { return out };
    for (name, def) in o.iter_mut() {
        let Some(terms) = def.get_mut("terms").and_then(|t| t.as_object_mut()) else {
            continue;
        };
        let part = terms.get("include").and_then(|i| i.get("partition")).and_then(|v| v.as_i64());
        let num =
            terms.get("include").and_then(|i| i.get("num_partitions")).and_then(|v| v.as_i64());
        let (Some(part), Some(num)) = (part, num) else { continue };
        let size = terms.get("size").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
        terms.remove("include");
        // ask for the whole term space, since which terms fall in the wanted
        // partition is not known until they are hashed
        terms.insert("size".into(), json!(65_536));
        out.push((name.clone(), part, num, size));
    }
    out
}

pub(crate) fn apply_partitions(result: &mut Value, parts: &[(String, i64, i64, usize)]) {
    for (name, part, num, size) in parts {
        let Some(buckets) = result.pointer_mut(&format!("/{name}/buckets")) else { continue };
        let Some(list) = buckets.as_array_mut() else { continue };
        list.retain(|b| b.get("key").map(|k| term_partition(k, *num) == *part).unwrap_or(false));
        list.truncate(*size);
    }
}

pub(crate) fn extract_bucket_orders(node: &mut Value) -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    let Some(o) = node.as_object_mut() else { return out };
    for (name, def) in o.iter_mut() {
        let Some(terms) = def.get_mut("terms") else { continue };
        let Some(order) = terms.get("order").cloned() else { continue };
        let Some(oo) = order.as_object() else { continue };
        let Some((key, dir)) = oo.iter().next() else { continue };
        if !key.contains('.') {
            continue;
        }
        let sub = key.split('.').next().unwrap_or("").to_string();
        let desc = dir.as_str().map(|d| d == "desc").unwrap_or(false);
        if let Some(o) = terms.as_object_mut() {
            o.remove("order");
        }
        out.push((name.clone(), sub, desc));
    }
    out
}

pub(crate) fn apply_bucket_orders(result: &mut Value, orders: &[(String, String, bool)]) {
    for (agg, sub, desc) in orders {
        let Some(buckets) = result.pointer_mut(&format!("/{agg}/buckets")) else { continue };
        let Some(list) = buckets.as_array_mut() else { continue };
        list.sort_by(|a, b| {
            let av = a.pointer(&format!("/{sub}/doc_count")).and_then(|v| v.as_i64()).unwrap_or(0);
            let bv = b.pointer(&format!("/{sub}/doc_count")).and_then(|v| v.as_i64()).unwrap_or(0);
            if *desc { bv.cmp(&av) } else { av.cmp(&bv) }
        });
    }
}

/// A string `missing` on a field the index holds no values for.
///
/// The columns a text substitute would need do not exist, so the aggregation
/// reads nothing and answers zero. Every document takes the same substitute
/// though, which makes the distinct count one whatever that value is -- so a
/// numeric stand-in gives the right answer through a column that does exist.
/// Only applied where the field is known to hold nothing at all.
pub(crate) fn substitute_unusable_missing(body: &mut Value, ctx: &Ctx) {
    let Some(o) = body.as_object_mut() else { return };
    if !matches!(o.get("missing"), Some(Value::String(_))) {
        return;
    }
    let Some(field) = o.get("field").and_then(|f| f.as_str()) else { return };
    let unobserved =
        ctx.kinds_complete && ctx.observed_kinds.get(field).map(|k| *k == 0).unwrap_or(true);
    if unobserved {
        o.insert("missing".into(), json!(0));
    }
}

pub(crate) fn rewrite_agg_fields(node: &mut Value, ctx: &Ctx) {
    match node {
        Value::Object(o) => {
            if let Some(Value::String(f)) = o.get("field") {
                // a field already named as the column it lives in is left as
                // it is; naming one that way is how a caller asks for the
                // analysed view rather than the stored one
                if f.starts_with(&format!("{}.", crate::store::DYN))
                    || f.starts_with(&format!("{}.", crate::store::RAW))
                {
                    for (_, v) in o.iter_mut() {
                        rewrite_agg_fields(v, ctx);
                    }
                    return;
                }
                // `_raw` carries both the untokenised strings and the numerics,
                // so it is the right column for every agg except one over an
                // explicitly analysed text field.
                // `title.keyword` is the raw view of `title`; but a field
                // called `keyword` under an object is a field of its own
                let base = match f.strip_suffix(".keyword") {
                    Some(parent)
                        if !matches!(ctx.mapping.type_of(parent), Some("object" | "nested")) =>
                    {
                        parent
                    }
                    _ => f,
                };
                // Both views carry the numerics, but resolving a purely numeric
                // path is measurably cheaper on `_dyn` -- `_raw` also holds a
                // string column for every path, which the lookup has to consider.
                // Strings must stay on `_raw`, whose values are untokenised.
                let numeric_only = std::env::var("BOOSTSEARCH_NO_NUMERIC_DYN_AGG").is_err()
                    && ctx
                        .observed_kinds
                        .get(base)
                        .map(|k| {
                            *k != 0 && k & (crate::store::KIND_STR | crate::store::KIND_DATE) == 0
                        })
                        .unwrap_or(false);
                let analyzed =
                    matches!(ctx.view(f, false), View::Dyn) && ctx.mapping.type_of(f).is_some();
                let prefix =
                    if analyzed || numeric_only { crate::store::DYN } else { crate::store::RAW };
                let rewritten = format!("{prefix}.{base}");
                o.insert("field".into(), json!(rewritten));
            }
            for (k, v) in o.iter_mut() {
                if k == "cardinality" {
                    substitute_unusable_missing(v, ctx);
                }
                rewrite_agg_fields(v, ctx);
            }
        }
        Value::Array(a) => {
            for v in a {
                rewrite_agg_fields(v, ctx);
            }
        }
        _ => {}
    }
}

/// BoostCore's aggregation model has no room for OpenSearch's `meta`, and it
/// spells the sub-aggregation key `aggs`. Strip one, normalise the other, and
/// remember the metadata so it can be put back on the response.
pub(crate) fn normalize_aggs(node: &mut Value, metas: &mut Vec<(String, Value)>, top: bool) {
    let Some(map) = node.as_object_mut() else { return };
    for (name, def) in map.iter_mut() {
        let Some(d) = def.as_object_mut() else { continue };
        if let Some(sub) = d.remove("aggregations") {
            d.insert("aggs".into(), sub);
        }
        if let Some(meta) = d.remove("meta")
            && top
        {
            metas.push((name.clone(), meta));
        }
        // `_term` and `_time` are the old spellings of `_key`, kept working
        // for the aggregations that were named before it was renamed
        for agg in d.values_mut() {
            let Some(order) = agg.get_mut("order").and_then(|o| o.as_object_mut()) else {
                continue;
            };
            for old in ["_term", "_time"] {
                if let Some(dir) = order.remove(old) {
                    order.insert("_key".into(), dir);
                }
            }
        }
        if let Some(sub) = d.get_mut("aggs") {
            normalize_aggs(sub, metas, false);
        }
    }
}

/// Recompute extended_stats moments the way OpenSearch does, from the raw
/// sums, so the last-bit float results agree.
pub(crate) fn recompute_extended_stats(v: &mut Value) {
    match v {
        Value::Object(o) => {
            let ready = o.contains_key("sum_of_squares")
                && o.contains_key("count")
                && o.contains_key("sum");
            if ready {
                let count = o.get("count").and_then(|x| x.as_f64()).unwrap_or(0.0);
                let sum = o.get("sum").and_then(|x| x.as_f64()).unwrap_or(0.0);
                let sq = o.get("sum_of_squares").and_then(|x| x.as_f64()).unwrap_or(0.0);
                if count > 0.0 {
                    let centred = sq - ((sum * sum) / count);
                    let var = centred / count;
                    let var_samp = if count > 1.0 { centred / (count - 1.0) } else { f64::NAN };
                    let sd = var.sqrt();
                    let sd_samp = var_samp.sqrt();
                    let sigma = o
                        .get("std_deviation_bounds")
                        .and_then(|b| b.get("upper"))
                        .and_then(|x| x.as_f64())
                        .and_then(|upper| {
                            let mean = sum / count;
                            let old_sd =
                                o.get("std_deviation").and_then(|x| x.as_f64()).unwrap_or(0.0);
                            if old_sd != 0.0 { Some((upper - mean) / old_sd) } else { None }
                        })
                        .map(|raw| (raw * 1e6).round() / 1e6) // undo float noise in the derivation
                        .unwrap_or(2.0);
                    let mean = sum / count;
                    o.insert("variance".into(), json!(var));
                    o.insert("variance_population".into(), json!(var));
                    o.insert("variance_sampling".into(), json!(var_samp));
                    o.insert("std_deviation".into(), json!(sd));
                    o.insert("std_deviation_population".into(), json!(sd));
                    o.insert("std_deviation_sampling".into(), json!(sd_samp));
                    o.insert(
                        "std_deviation_bounds".into(),
                        json!({
                            "upper": mean + sd * sigma,
                            "lower": mean - sd * sigma,
                            "upper_population": mean + sd * sigma,
                            "lower_population": mean - sd * sigma,
                            "upper_sampling": mean + sd_samp * sigma,
                            "lower_sampling": mean - sd_samp * sigma,
                        }),
                    );
                }
            }
            for (_, child) in o.iter_mut() {
                recompute_extended_stats(child);
            }
        }
        Value::Array(a) => {
            for x in a {
                recompute_extended_stats(x);
            }
        }
        _ => {}
    }
}

pub(crate) fn reattach_meta(result: &mut Value, metas: &[(String, Value)]) {
    for (name, meta) in metas {
        if let Some(slot) = result.get_mut(name)
            && let Some(o) = slot.as_object_mut()
        {
            o.insert("meta".into(), meta.clone());
        }
    }
}

pub(crate) fn inject_doc_count_helpers(node: &mut Value) {
    let Some(o) = node.as_object_mut() else { return };
    for (_, def) in o.iter_mut() {
        let Some(d) = def.as_object_mut() else { continue };
        let is_bucket = d.keys().any(|k| {
            matches!(k.as_str(), "terms" | "histogram" | "date_histogram" | "range" | "filters")
        });
        let slot = if d.contains_key("aggregations") { "aggregations" } else { "aggs" };
        if let Some(sub) = d.get_mut(slot) {
            inject_doc_count_helpers(sub);
        }
        if !is_bucket {
            continue;
        }
        let subs = d.entry(slot).or_insert_with(|| json!({}));
        if let Some(m) = subs.as_object_mut() {
            m.insert(DC_SUM.into(), json!({"sum": {"field": "_doc_count"}}));
            m.insert(DC_CNT.into(), json!({"value_count": {"field": "_doc_count"}}));
        }
    }
}

pub(crate) fn apply_doc_counts(node: &mut Value) {
    match node {
        Value::Object(o) => {
            if let Some(Value::Array(buckets)) = o.get_mut("buckets") {
                for b in buckets.iter_mut() {
                    let sum = b.pointer(&format!("/{DC_SUM}/value")).and_then(|v| v.as_f64());
                    let cnt = b.pointer(&format!("/{DC_CNT}/value")).and_then(|v| v.as_f64());
                    if let (Some(sum), Some(cnt)) = (sum, cnt) {
                        let base = b.get("doc_count").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        b["doc_count"] = json!((base + sum - cnt).max(0.0) as u64);
                    }
                    if let Some(m) = b.as_object_mut() {
                        m.remove(DC_SUM);
                        m.remove(DC_CNT);
                    }
                }
                // the correction can reorder buckets a count-ordered agg sorted
                // before it was applied
                if let Some(Value::Array(buckets)) = o.get_mut("buckets") {
                    buckets.sort_by(|a, b| {
                        let get =
                            |v: &Value| v.get("doc_count").and_then(|x| x.as_u64()).unwrap_or(0);
                        get(b).cmp(&get(a))
                    });
                }
            }
            for (_, v) in o.iter_mut() {
                apply_doc_counts(v);
            }
        }
        Value::Array(a) => {
            for v in a {
                apply_doc_counts(v);
            }
        }
        _ => {}
    }
}

/// Take include/exclude off the aggregations whose field cannot honour them.
pub(crate) fn strip_untranslatable_term_filters(node: &mut Value, ctx: &Ctx) {
    match node {
        Value::Object(o) => {
            if let Some(terms) = o.get_mut("terms").and_then(|t| t.as_object_mut()) {
                let field = terms.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
                let base = match field.strip_suffix(".keyword") {
                    Some(parent)
                        if !matches!(ctx.mapping.type_of(parent), Some("object" | "nested")) =>
                    {
                        parent
                    }
                    _ => field.as_str(),
                };
                if term_filter_needs_translating(ctx.mapping.type_of(base)) {
                    terms.remove("include");
                    terms.remove("exclude");
                }
            }
            for (_, v) in o.iter_mut() {
                strip_untranslatable_term_filters(v, ctx);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|v| strip_untranslatable_term_filters(v, ctx)),
        _ => {}
    }
}
