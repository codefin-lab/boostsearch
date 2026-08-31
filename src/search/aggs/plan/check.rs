//! What a request may ask an aggregation for, checked before it is parsed.

use super::*;

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
