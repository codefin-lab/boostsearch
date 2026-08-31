//! A term, as the index holds it, and the sets a query names.

use super::*;

/// Rewrite a value written as an IP into the form the field was indexed in.
pub(crate) fn ip_value(ctx: &Ctx, field: &str, v: &Value) -> Value {
    match ctx.mapping.type_of(field) {
        Some("ip") => match v.as_str().and_then(crate::store::canonical_ip) {
            Some(c) => Value::String(c),
            None => v.clone(),
        },
        // a date is a number in the index, and a query has to name it the
        // same way whatever spelling it was written in
        Some(ty @ ("date" | "date_nanos")) => {
            let fmt = ctx.mapping.field_option(field, "format");
            let fmt = fmt.as_ref().and_then(|v| v.as_str());
            match crate::store::date_number(v, fmt, ty == "date_nanos") {
                Some(n) => Value::Number(n.into()),
                None => v.clone(),
            }
        }
        _ => v.clone(),
    }
}

/// `term` on an `ip` field accepts a CIDR block, which names a range of
/// addresses rather than one of them.
pub(crate) fn ip_term_query(
    ctx: &Ctx,
    field: &str,
    f: Field,
    path: &str,
    v: &Value,
) -> Option<Box<dyn Query>> {
    if ctx.mapping.type_of(field) != Some("ip") {
        return None;
    }
    let s = v.as_str()?;
    if let Some((lo, hi)) = crate::store::canonical_cidr(s) {
        let mut l = Term::from_field_json_path(f, path, true);
        l.append_type_and_str(&lo);
        let mut h = Term::from_field_json_path(f, path, true);
        h.append_type_and_str(&hi);
        return Some(Box::new(RangeQuery::new(
            Bound::Included(l),
            Bound::Included(h),
        )));
    }
    Some(any_of(term_for(f, path, &ip_value(ctx, field, v))))
}

pub(crate) fn term_for(field: Field, path: &str, v: &Value) -> Vec<Term> {
    let base = Term::from_field_json_path(field, path, true);
    match v {
        Value::String(s) => {
            let mut t = base.clone();
            t.append_type_and_str(s);
            let mut out = vec![t];
            // strings that were indexed as dates need a date term too
            if let Some(dt) = parse_datetime(s) {
                let mut d = base;
                d.append_type_and_fast_value(dt);
                out.push(d);
            }
            out
        }
        Value::Bool(b) => {
            let mut t = base;
            t.append_type_and_fast_value(*b);
            vec![t]
        }
        Value::Number(n) => {
            let mut out = Vec::new();
            // 401.0 and 401 name the same value; whichever form was indexed,
            // either spelling of the query has to find it
            let whole = n.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 9.007e15);
            let as_i64 = n.as_i64().or_else(|| whole.map(|f| f as i64));
            let as_u64 = n.as_u64().or_else(|| whole.filter(|f| *f >= 0.0).map(|f| f as u64));
            if let Some(i) = as_i64 {
                let mut t = base.clone();
                t.append_type_and_fast_value(i);
                out.push(t);
            }
            if let Some(u) = as_u64 {
                let mut t = base.clone();
                t.append_type_and_fast_value(u);
                out.push(t);
            }
            if let Some(f) = n.as_f64() {
                let mut t = base.clone();
                t.append_type_and_fast_value(f);
                out.push(t);
            }
            out
        }
        _ => vec![],
    }
}

pub fn parse_datetime(s: &str) -> Option<boostcore::DateTime> {
    use boostcore::time::format_description::well_known::Rfc3339;
    boostcore::time::OffsetDateTime::parse(s, &Rfc3339).ok().map(boostcore::DateTime::from_utc)
}

pub(crate) fn any_of(terms: Vec<Term>) -> Box<dyn Query> {
    match terms.len() {
        0 => Box::new(EmptyQuery),
        1 => Box::new(TermQuery::new(terms.into_iter().next().unwrap(), IndexRecordOption::Basic)),
        _ => Box::new(BooleanQuery::new(
            terms
                .into_iter()
                .map(|t| {
                    (Occur::Should, Box::new(TermQuery::new(t, IndexRecordOption::Basic)) as Box<dyn Query>)
                })
                .collect(),
        )),
    }
}
