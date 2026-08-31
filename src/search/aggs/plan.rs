//! Who answers which aggregation, and what has to happen to the request
//! before BoostCore is given it.

use super::*;
use crate::search::*;

/// Reject a numeric metric over a string field the way OpenSearch does.
pub(crate) fn check_agg_types(node: &Value, ctx: &Ctx) -> std::result::Result<(), Response> {
    check_agg_node(node, ctx, "")
}

/// Numeric parameter bounds OpenSearch enforces; `owner` is the aggregation
/// name the message has to quote.
pub(crate) fn check_agg_params(
    name: &str,
    def: &Value,
    owner: &str,
) -> std::result::Result<(), Response> {
    let bad = |param: &str, got: f64, bound: &str| {
        err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!(
                "[{param}] must be greater than {bound}. Found [{}] in [{owner}]",
                if got.fract() == 0.0 && param == "precisionThreshold" {
                    format!("{}", got as i64)
                } else {
                    format!("{got:?}")
                }
            ),
        )
    };
    let num = |k: &str| def.get(k).and_then(|v| v.as_f64());
    match name {
        "extended_stats" => {
            if let Some(v) = num("sigma")
                && v < 0.0
            {
                return Err(bad("sigma", v, "or equal to 0"));
            }
        }
        "cardinality" => {
            if let Some(v) = num("precision_threshold")
                && v < 0.0
            {
                return Err(bad("precisionThreshold", v, "or equal to 0"));
            }
        }
        "percentiles" | "median_absolute_deviation" => {
            if let Some(d) = def.pointer("/hdr/number_of_significant_value_digits")
                && !matches!(d.as_i64(), Some(0..=5))
            {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    "[numberOfSignificantValueDigits] must be between 0 and 5",
                ));
            }
            // `percents` names which percentiles to report, so an empty or
            // unreadable list leaves nothing to compute
            if let Some(p) = def.get("percents") {
                let ok = p
                    .as_array()
                    .map(|a| !a.is_empty() && a.iter().all(|v| v.as_f64().is_some()))
                    .unwrap_or(false);
                if !ok {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "x_content_parse_exception",
                        "[percents] must be a non-empty list of numbers",
                    ));
                }
            }
            if let Some(v) = num("compression")
                && v <= 0.0
            {
                return Err(bad("compression", v, "0"));
            }
            // the tdigest sketch takes its own compression, and admits 0
            if let Some(v) = def.pointer("/tdigest/compression").and_then(|v| v.as_f64())
                && v < 0.0
            {
                return Err(bad("compression", v, "or equal to 0"));
            }
        }
        "moving_fn" | "moving_avg" => {
            // a window of zero or fewer has nothing to average over
            if let Some(v) = num("window")
                && v < 1.0
            {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    "[window] must be a positive, non-zero integer.",
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

/// Walk the aggregation tree applying only the checks that need no mapping.
pub(crate) fn check_agg_bounds(node: &Value, owner: &str) -> std::result::Result<(), Response> {
    let Some(o) = node.as_object() else { return Ok(()) };
    for (name, def) in o {
        check_agg_params(name, def, owner)?;
        let next_owner = if owner.is_empty() { name.as_str() } else { owner };
        check_agg_bounds(def, next_owner)?;
    }
    Ok(())
}

pub(crate) fn check_agg_node(
    node: &Value,
    ctx: &Ctx,
    owner: &str,
) -> std::result::Result<(), Response> {
    let Some(o) = node.as_object() else { return Ok(()) };
    for (name, def) in o {
        check_agg_params(name, def, owner)?;
        // a flat_object holds whatever it was given, so there is nothing of a
        // known type under it to aggregate over
        if let Some(field) = def.get("field").and_then(|f| f.as_str()) {
            let mut walked = String::new();
            for part in field.split('.') {
                walked =
                    if walked.is_empty() { part.to_string() } else { format!("{walked}.{part}") };
                if ctx.mapping.type_of(&walked) == Some("flat_object") && walked != field {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        format!(
                            "Field [{field}] of type [flat_object] is not supported for \
                             aggregation [{name}]"
                        ),
                    ));
                }
            }
        }
        // `terms` is also the name of a query, which appears inside filter
        // aggregations and inside multi_terms; only an object made entirely of
        // terms-aggregation options is one of those
        const TERMS_AGG_OPTIONS: &[&str] = &[
            "field",
            "script",
            "size",
            "shard_size",
            "order",
            "include",
            "exclude",
            "min_doc_count",
            "shard_min_doc_count",
            "missing",
            "execution_hint",
            "collect_mode",
            "value_type",
            "format",
            "show_term_doc_count_error",
        ];
        if name == "terms" && def.get("field").is_none() && def.get("script").is_none() {
            let all_options = def
                .as_object()
                .map(|o| !o.is_empty() && o.keys().all(|k| TERMS_AGG_OPTIONS.contains(&k.as_str())))
                .unwrap_or(false);
            if all_options {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    "Required one of fields [field, script], but none were specified. ",
                ));
            }
        }
        if name == "terms" {
            for pass in ["include", "exclude"] {
                if !matches!(def.get(pass), Some(Value::String(_))) {
                    continue;
                }
                let field = def.get("field").and_then(|f| f.as_str()).unwrap_or("");
                let base = field.strip_suffix(".keyword").unwrap_or(field);
                if !matches!(
                    ctx.mapping.type_of(base),
                    None | Some("keyword" | "text" | "wildcard")
                ) {
                    return Err(err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "illegal_argument_exception",
                        format!(
                            "Aggregation [{owner}] cannot support regular expression style \
                             include/exclude settings as they can only be applied to string \
                             fields. Use an array of values for include/exclude clauses"
                        ),
                    ));
                }
            }
        }
        if NUMERIC_AGGS.contains(&name.as_str())
            && let Some(f) = def.get("field").and_then(|v| v.as_str())
        {
            // a field the mapping never named is still a text field if
            // text is all it has ever held
            let dynamic_text = ctx.mapping.type_of(f).is_none()
                && ctx.kinds_complete
                && ctx.observed_kinds.get(f).map(|k| *k == crate::store::KIND_STR).unwrap_or(false);
            if matches!(ctx.mapping.type_of(f), Some("text") | Some("keyword")) || dynamic_text {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!(
                        "Field [{f}] of type [{}] is not supported for aggregation [{name}]",
                        ctx.mapping.type_of(f).unwrap_or("text")
                    ),
                ));
            }
        }
        // at the top level the key is the user's name for the aggregation
        let next_owner = if owner.is_empty() { name.as_str() } else { owner };
        check_agg_node(def, ctx, next_owner)?;
    }
    Ok(())
}

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
                let base = f.strip_suffix(".keyword").unwrap_or(f);
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
    query: &dyn boostcore::query::Query,
) -> boostcore::Result<usize> {
    let weight = query.weight(boostcore::query::EnableScoring::disabled_from_searcher(searcher))?;
    let mut total = 0usize;
    for reader in searcher.segment_readers() {
        total += weight.count(reader)? as usize;
    }
    Ok(total)
}

/// `search.max_buckets` caps how many buckets one request may build.
///
/// The limit is counted over the whole answer, sub-buckets included, which is
/// what makes a nested terms aggregation the expensive one.
pub(crate) fn check_max_buckets(
    store: &Store,
    aggs: &Option<Value>,
) -> std::result::Result<(), Response> {
    if let Some(limit) = store
        .cluster_setting("search.max_buckets")
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
    {
        fn count_buckets(node: &Value) -> u64 {
            match node {
                Value::Object(o) => o
                    .iter()
                    .map(|(k, v)| {
                        let here = if k == "buckets" {
                            match v {
                                Value::Array(a) => a.len() as u64,
                                Value::Object(b) => b.len() as u64,
                                _ => 0,
                            }
                        } else {
                            0
                        };
                        here + count_buckets(v)
                    })
                    .sum(),
                Value::Array(a) => a.iter().map(count_buckets).sum(),
                _ => 0,
            }
        }
        let built = aggs.as_ref().map(count_buckets).unwrap_or(0);
        if built > limit {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "too_many_buckets_exception",
                format!(
                    "Trying to create too many buckets. Must be less than or equal to: \
                     [{limit}] but was [{built}]. This limit can be set by changing the \
                     [search.max_buckets] cluster level setting."
                ),
            ));
        }
    }
    Ok(())
}

/// Turn what the shards collected into the answer a client reads.
///
/// The shards hand back intermediate results; combining them is BoostCore's
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
                    let types: std::collections::HashMap<String, String> = targets
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
                }
                if weighted {
                    apply_doc_counts(&mut v);
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

/// Run the aggregations BoostCore could not parse, each as its own search.
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
    // BoostCore has `filter` but not `filters`; peel those out and run them
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
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            format!("{kind} aggregation [{name}] must be declared inside of another aggregation"),
        ));
    }
    let mut filters_aggs: Vec<(String, Value)> = Vec::new();
    if let Some(Value::Object(o)) = agg_json.as_mut() {
        let names: Vec<String> = o
            .iter()
            .filter(|(_, def)| {
                // anything under it that has to be run here drags the whole
                // aggregation out of BoostCore's hands with it
                peelable(def)
                    || def.get("filters").is_some()
                    || def.get("missing").is_some()
                    || def.get("median_absolute_deviation").is_some()
                    // percentiles answer a different question from BoostCore's
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
                    // BoostCore's own `filter` agg only speaks its query-string
                    // dialect, so run singular filters through our query builder
                    || def.get("filter").is_some()
                    || def.get("composite").is_some()
                    || def.get("multi_terms").is_some()
                    || def.get("rare_terms").is_some()
                    || def.get("nested").is_some()
                    || def.get("reverse_nested").is_some()
                    || def.get("sampler").is_some()
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
                    // index holds is a plain histogram, which BoostCore runs
                    || def.get("date_histogram").map(walked_here).unwrap_or(false)
                    // a range field holds no single value to bucket a document
                    // by, so BoostCore's histogram sees nothing there at all
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
/// ones BoostCore can parse, so each set can take the path that suits it.
pub(crate) fn split_peelable(sub_aggs: &Option<Value>) -> (Option<Value>, Option<Value>) {
    let Some(o) = sub_aggs.as_ref().and_then(|s| s.as_object()) else {
        return (None, sub_aggs.clone());
    };
    let (mine, theirs): (Vec<_>, Vec<_>) = o.iter().partition(|(_, d)| peelable(d));
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
pub(crate) fn peelable(def: &Value) -> bool {
    peelable_here(def)
        || def
            .get("aggs")
            .or_else(|| def.get("aggregations"))
            .and_then(|s| s.as_object())
            .map(|o| o.values().any(peelable))
            .unwrap_or(false)
}

/// Is this an aggregation BoostCore has no parser for, which has to be computed
/// a bucket at a time here instead?
pub(crate) fn peelable_here(def: &Value) -> bool {
    const OWN: &[&str] = &[
        "missing",
        "median_absolute_deviation",
        "filter",
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
        "sampler",
        "diversified_sampler",
    ];
    OWN.iter().any(|k| def.get(k).is_some())
        || def.get("date_histogram").map(walked_here).unwrap_or(false)
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
/// that column is the same bucketing -- and BoostCore walks it in one pass
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
/// including the ones BoostCore cannot parse, which are run here against the
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
    let (mine, theirs): (Vec<_>, Vec<_>) = subs.iter().partition(|(_, d)| peelable(d));
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

pub(crate) fn filtered_count(
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
            index: &g.index,
            max_terms_count: g.max_terms_count(),
            max_regex_length: g.max_regex_length(),
            allow_expensive: crate::search::expensive_allowed(store),
            observed_kinds: &g.observed_kinds,
            kinds_complete: g.kinds_complete,
            stats: &g.stats,
        };
        let q = crate::query::build(&ctx, query_json)
            .map_err(|e| err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string()))?;
        let searcher = g.reader.searcher();
        total += searcher.search(&q, &Count).map_err(|e| {
            err(StatusCode::BAD_REQUEST, "search_phase_execution_exception", e.to_string())
        })? as u64;

        if let Some(sa) = sub_aggs {
            let mut rewritten = sa.clone();
            let mut ignored = Vec::new();
            normalize_aggs(&mut rewritten, &mut ignored, false);
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
    let sub = match (acc, req) {
        (Some(a), Some(r)) => a
            .into_final_result(r, Default::default())
            .ok()
            .and_then(|v| serde_json::to_value(v).ok()),
        _ => None,
    };
    Ok((total, sub))
}

/// Take include/exclude off the aggregations whose field cannot honour them.
pub(crate) fn strip_untranslatable_term_filters(node: &mut Value, ctx: &Ctx) {
    match node {
        Value::Object(o) => {
            if let Some(terms) = o.get_mut("terms").and_then(|t| t.as_object_mut()) {
                let field = terms.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
                let base = field.strip_suffix(".keyword").unwrap_or(&field);
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
