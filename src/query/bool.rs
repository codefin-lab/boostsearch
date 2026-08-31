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

    let msm = body.get("minimum_should_match").and_then(parse_msm);
    let required = match msm {
        Some(n) => resolve_msm(n, should_count),
        None if !has_must_or_filter && should_count > 0 => 1,
        _ => 0,
    };
    Ok(Box::new(BooleanQuery::with_minimum_required_clauses(clauses, required)))
}

pub(crate) fn parse_msm(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

pub(crate) fn resolve_msm(n: i64, should_count: usize) -> usize {
    if n < 0 { (should_count as i64 + n).max(0) as usize } else { (n as usize).min(should_count) }
}
