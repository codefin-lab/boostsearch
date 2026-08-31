//! What a request may ask for, checked before any index is opened.

use super::*;

pub(crate) fn body_or_param<'a>(body: &'a Value, p: &'a Params, key: &str) -> Option<Value> {
    body.get(key).cloned().or_else(|| p.get(key).map(|v| json!(v)))
}

/// Request-level parameter validation, shared by search and msearch.
pub fn validate_params(body: &Value, p: &Params) -> std::result::Result<(), Response> {
    // an int total is only meaningful when the count is exact
    if p.get("rest_total_hits_as_int").map(|v| v == "true").unwrap_or(false) {
        let track = body
            .get("track_total_hits")
            .cloned()
            .or_else(|| p.get("track_total_hits").map(|v| json!(v)));
        let inaccurate = match &track {
            Some(Value::Bool(false)) => None,
            Some(Value::Number(n)) => Some(n.to_string()),
            Some(Value::String(s)) if s == "false" => None,
            Some(Value::String(s)) if s != "true" => Some(s.clone()),
            _ => None,
        };
        if let Some(got) = inaccurate {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!(
                    "[rest_total_hits_as_int] cannot be used if the tracking of total hits is not accurate, got {got}"
                ),
            ));
        }
    }
    Ok(())
}

/// The ceilings a search has to stay under, all of them index settings.
///
/// They exist because each one costs memory on the node answering, so the
/// complaint says which setting to raise rather than only that the request was
/// refused.
pub(crate) fn check_limits(
    store: &Store,
    targets: &[String],
    body: &Value,
    p: &Params,
    from: usize,
    size: usize,
) -> std::result::Result<(), Response> {
    let setting = |key: &str, default: u64| -> u64 {
        targets
            .iter()
            .filter_map(|n| store.get(n))
            .filter_map(|st| st.read().numeric_setting(key))
            .max()
            .unwrap_or(default)
    };
    let bad = |reason: String| err(StatusCode::BAD_REQUEST, "illegal_argument_exception", reason);

    // a scroll has to know how much is left to walk
    if p.contains_key("scroll") && matches!(body.get("track_total_hits"), Some(Value::Bool(false)))
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "disabling [track_total_hits] is not allowed in a scroll context",
        ));
    }
    // a scroll walks the whole result set in order; collapsing rewrites what
    // that order even is, so the two cannot be asked for together
    if p.contains_key("scroll") && body.get("collapse").is_some() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "search_phase_execution_exception",
            "cannot use `collapse` in a scroll context",
        ));
    }
    // a page picked up after a marker has to be picked up in the same order
    // the groups are in, which means sorting by the very field they collapse on
    if let (Some(field), true) = (
        body.pointer("/collapse/field").and_then(|v| v.as_str()),
        body.get("search_after").is_some(),
    ) {
        let keys: Vec<Value> = match body.get("sort") {
            Some(Value::Array(a)) => a.clone(),
            Some(other) => vec![other.clone()],
            None => Vec::new(),
        };
        let named = match keys.first() {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Object(o)) => o.keys().next().cloned().unwrap_or_default(),
            _ => String::new(),
        };
        if keys.len() != 1 || named != field {
            return Err(bad(
                "collapse field and sort field must be the same when use `collapse` in \
                 conjunction with `search_after`"
                    .into(),
            ));
        }
    }
    // a collapse inside inner hits may name a field and nothing else: there is
    // no third level to collapse, and no hits to fetch under one
    let inners = match body.pointer("/collapse/inner_hits") {
        Some(Value::Array(a)) => a.clone(),
        Some(other) => vec![other.clone()],
        None => Vec::new(),
    };
    for inner in &inners {
        let Some(second) = inner.get("collapse").and_then(|c| c.as_object()) else { continue };
        if second.keys().any(|k| k != "field") {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "parse_exception",
                "Invalid token in the inner collapse",
            ));
        }
    }
    // rescoring reorders the top of the result set, which is the very thing
    // collapsing has already decided
    if body.get("rescore").is_some() && body.get("collapse").is_some() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "search_phase_execution_exception",
            "cannot use `collapse` in conjunction with `rescore`",
        ));
    }

    // a page is counted from the front, so there is no such thing as starting
    // before it
    let negative = body
        .get("from")
        .and_then(|v| v.as_i64())
        .or_else(|| p.get("from").and_then(|v| v.parse::<i64>().ok()))
        .map(|v| v < 0)
        .unwrap_or(false);
    if negative {
        return Err(bad("[from] parameter cannot be negative".into()));
    }
    // a size past what an int holds never reaches the window check: it is not
    // a number the request could have meant
    let too_wide = body
        .get("size")
        .and_then(|v| v.as_u64())
        .or_else(|| p.get("size").and_then(|v| v.parse::<u64>().ok()))
        .filter(|v| *v > i32::MAX as u64);
    if let Some(v) = too_wide {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "input_coercion_exception",
            format!("Numeric value ({v}) out of range of int"),
        ));
    }

    let window = setting("max_result_window", 10_000);
    if p.contains_key("scroll") {
        // a scroll reads a batch at a time, and a batch costs what a window
        // costs, so the same ceiling holds
        if size as u64 > window {
            return Err(bad(format!(
                "Batch size is too large, size must be less than or equal to: [{window}] but was \
                 [{size}]. Scroll batch sizes cost as much memory as result windows so they are \
                 controlled by the [index.max_result_window] index level setting."
            )));
        }
    } else if (from + size) as u64 > window {
        let total = from + size;
        return Err(bad(format!(
            "Result window is too large, from + size must be less than or equal to: [{window}] \
             but was [{total}]. See the scroll api for a more efficient way to request large data \
             sets."
        )));
    }

    // rescoring re-reads a window's worth of hits, so it has its own ceiling
    let windows = match body.get("rescore") {
        Some(Value::Array(a)) => a.clone(),
        Some(other) => vec![other.clone()],
        None => Vec::new(),
    };
    for r in windows {
        let want = r.get("window_size").and_then(|v| v.as_u64()).unwrap_or(0);
        if want > window {
            return Err(bad(format!(
                "Rescore window [{want}] is too large. It must be less than [{window}]."
            )));
        }
    }

    let counted = |key: &str| -> usize {
        body.get(key)
            .map(|v| match v {
                Value::Array(a) => a.len(),
                Value::Object(o) => o.len(),
                _ => 0,
            })
            .unwrap_or(0)
    };
    let docvalues = setting("max_docvalue_fields_search", 100);
    let n = counted("docvalue_fields");
    if n as u64 > docvalues {
        return Err(bad(format!(
            "Trying to retrieve too many docvalue_fields. Must be less than or equal to: \
             [{docvalues}] but was [{n}]. This limit can be set by changing the \
             [index.max_docvalue_fields_search] index level setting."
        )));
    }
    let scripts = setting("max_script_fields", 32);
    let n = counted("script_fields");
    if n as u64 > scripts {
        return Err(bad(format!(
            "Trying to retrieve too many script_fields. Must be less than or equal to: \
             [{scripts}] but was [{n}]. This limit can be set by changing the \
             [index.max_script_fields] index level setting."
        )));
    }
    Ok(())
}

pub(crate) fn as_i64(v: Option<Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

pub(crate) fn as_usize(v: Option<Value>) -> Option<usize> {
    match v? {
        Value::Number(n) => n.as_u64().map(|x| x as usize),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}
