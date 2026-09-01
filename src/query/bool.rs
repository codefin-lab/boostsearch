//! `bool`, and what `minimum_should_match` means.

use super::*;

pub(crate) fn build_bool(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    let mut should_count = 0usize;
    let mut has_must_or_filter = false;

    for (key, occur) in [
        ("must", Occur::Must),
        ("filter", Occur::Must),
        ("should", Occur::Should),
        ("must_not", Occur::MustNot),
    ] {
        let Some(v) = body.get(key) else { continue };
        let list: Vec<Value> = match v {
            Value::Array(a) => a.clone(),
            Value::Null => vec![],
            other => vec![other.clone()],
        };
        for item in list {
            let sub = build(ctx, &item)?;
            let sub: Box<dyn Query> =
                if key == "filter" { Box::new(ConstScore::new(sub, 0.0)) } else { sub };
            if occur == Occur::Should {
                should_count += 1;
            }
            if key == "must" || key == "filter" {
                has_must_or_filter = true;
            }
            clauses.push((occur, sub));
        }
    }

    if clauses.is_empty() {
        return Ok(Box::new(AllQuery));
    }
    // a bool with only must_not still needs a positive base
    if clauses.iter().all(|(o, _)| *o == Occur::MustNot) {
        clauses.push((Occur::Must, Box::new(AllQuery)));
    }

    let msm = msm_required(body.get("minimum_should_match"), should_count);
    let required = match msm {
        Some(n) => n,
        None if !has_must_or_filter && should_count > 0 => 1,
        _ => 0,
    };
    Ok(Box::new(BooleanQuery::with_minimum_required_clauses(clauses, required)))
}

/// How many of `n` optional clauses have to hold, as `minimum_should_match`
/// spells it.
///
/// A number is a count, a negative number is a count to leave out, a
/// percentage is a share of the clauses -- rounded down, or up from the far
/// end for a negative one -- and `N<spec` says the spec only applies once
/// there are more than N clauses, with several such conditions space-apart.
pub(crate) fn msm_required(spec: Option<&Value>, n: usize) -> Option<usize> {
    let text = match spec? {
        Value::Number(number) => number.to_string(),
        Value::String(s) => s.trim().to_string(),
        _ => return None,
    };
    let one = |part: &str| -> Option<i64> {
        let part = part.trim();
        if let Some(percent) = part.strip_suffix('%') {
            let share: f64 = percent.trim().parse().ok()?;
            let counted = (share.abs() / 100.0 * n as f64).floor() as i64;
            return Some(if share < 0.0 { n as i64 - counted } else { counted });
        }
        let count: i64 = part.parse().ok()?;
        Some(if count < 0 { n as i64 + count } else { count })
    };
    // conditions are tried in order, and the last whose threshold the clause
    // count passes is the one that holds; none passing means all clauses
    if text.contains('<') {
        let mut required: Option<i64> = None;
        for condition in text.split_whitespace() {
            let (threshold, rule) = condition.split_once('<')?;
            let threshold: usize = threshold.trim().parse().ok()?;
            if n > threshold {
                required = Some(one(rule)?);
            }
        }
        let required = required.unwrap_or(n as i64);
        return Some(required.clamp(0, n as i64) as usize);
    }
    Some(one(&text)?.clamp(0, n as i64) as usize)
}

pub(crate) fn resolve_msm(n: i64, should_count: usize) -> usize {
    if n < 0 { (should_count as i64 + n).max(0) as usize } else { (n as usize).min(should_count) }
}
