//! An expression rendered as Painless, for the questions an aggregation
//! cannot ask of a bare field.
//!
//! SQL groups by a field and aggregates a field, and this planner could only
//! do that: `GROUP BY MONTH(placed)` was refused with `cannot group by`, and
//! `SUM(CASE WHEN ... THEN 1 ELSE 0 END)` with `sum needs a field`. Both are
//! ordinary SQL, both are answered by the reference, and both are the same
//! gap -- an aggregation may read a script instead of a field, and nothing
//! here wrote one.
//!
//! Only what can be rendered exactly is rendered. Anything else returns
//! `None` and the planner refuses as it did before, which keeps a query that
//! used to be refused from quietly becoming a wrong answer.

use super::ast::{Condition, Expr};

/// A field, read from the column store.
fn field(name: &str) -> String {
    format!("doc['{}'].value", name.replace('\'', ""))
}

fn literal(v: &Expr) -> Option<String> {
    Some(match v {
        Expr::Number(n) => {
            if n.fract() == 0.0 && n.abs() < 9e15 {
                format!("{}L", *n as i64)
            } else {
                format!("{n}")
            }
        }
        Expr::Text(t) => format!("'{}'", t.replace('\\', "\\\\").replace('\'', "\\'")),
        Expr::Boolean(b) => b.to_string(),
        _ => return None,
    })
}

/// The Painless for one expression, or `None` if it cannot be written exactly.
pub(crate) fn painless(e: &Expr) -> Option<String> {
    Some(match e {
        Expr::Field(f) => field(f),
        Expr::Number(_) | Expr::Text(_) | Expr::Boolean(_) => literal(e)?,
        Expr::Negate(inner) => format!("(-{})", painless(inner)?),
        Expr::Binary { op, left, right } => {
            let (l, r) = (painless(left)?, painless(right)?);
            match op.as_str() {
                "+" | "-" | "*" | "/" | "%" => format!("({l} {op} {r})"),
                // SQL concatenates with `||`, Java with `+`
                "||" => format!("({l} + {r})"),
                _ => return None,
            }
        }
        Expr::Case { whens, otherwise } => {
            let mut out = match otherwise {
                Some(e) => painless(e)?,
                None => "null".to_string(),
            };
            // written from the last branch back, so the first WHEN is outermost
            for (cond, then) in whens.iter().rev() {
                out = format!("({} ? {} : {})", condition(cond)?, painless(then)?, out);
            }
            out
        }
        Expr::Call { name, args } => call(&name.to_lowercase(), args)?,
        _ => return None,
    })
}

fn call(name: &str, args: &[Expr]) -> Option<String> {
    let first = || args.first().and_then(painless);
    Some(match name {
        // the parts of a date
        "year" => format!("{}.getYear()", first()?),
        "month" | "month_of_year" | "monthofyear" => format!("{}.getMonthValue()", first()?),
        "dayofmonth" | "day_of_month" | "day" => format!("{}.getDayOfMonth()", first()?),
        "dayofyear" | "day_of_year" => format!("{}.getDayOfYear()", first()?),
        "hour" | "hour_of_day" | "hourofday" => format!("{}.getHour()", first()?),
        "minute" | "minute_of_hour" | "minuteofhour" => format!("{}.getMinute()", first()?),
        "second" | "second_of_minute" | "secondofminute" => format!("{}.getSecond()", first()?),
        "quarter" => format!("(({}.getMonthValue() - 1) / 3 + 1)", first()?),
        // arithmetic
        "abs" => format!("Math.abs({})", first()?),
        "ceil" | "ceiling" => format!("Math.ceil({})", first()?),
        "floor" => format!("Math.floor({})", first()?),
        "round" => format!("Math.round({})", first()?),
        "sqrt" => format!("Math.sqrt({})", first()?),
        "exp" => format!("Math.exp({})", first()?),
        "log" => format!("Math.log({})", first()?),
        "log10" => format!("Math.log10({})", first()?),
        "sign" | "signum" => format!("Math.signum({})", first()?),
        "pow" | "power" => {
            format!("Math.pow({}, {})", first()?, painless(args.get(1)?)?)
        }
        // text
        "upper" => format!("{}.toUpperCase()", first()?),
        "lower" => format!("{}.toLowerCase()", first()?),
        "length" => format!("{}.length()", first()?),
        _ => return None,
    })
}

/// The Painless for a condition, for the branches of a `CASE`.
pub(crate) fn condition(c: &Condition) -> Option<String> {
    Some(match c {
        Condition::And(a, b) => format!("({} && {})", condition(a)?, condition(b)?),
        Condition::Or(a, b) => format!("({} || {})", condition(a)?, condition(b)?),
        Condition::Not(inner) => format!("(!{})", condition(inner)?),
        Condition::Compare { left, op, right } => {
            let (l, r) = (painless(left)?, painless(right)?);
            let op = match op.as_str() {
                "=" | "==" => "==",
                "<>" | "!=" => "!=",
                ">" | ">=" | "<" | "<=" => op.as_str(),
                _ => return None,
            };
            format!("({l} {op} {r})")
        }
        Condition::Between { value, low, high, negated } => {
            let v = painless(value)?;
            let inside = format!("({v} >= {} && {v} <= {})", painless(low)?, painless(high)?);
            if *negated { format!("(!{inside})") } else { inside }
        }
        Condition::In { value, options, negated } => {
            let v = painless(value)?;
            let mut any = String::new();
            for (i, o) in options.iter().enumerate() {
                if i > 0 {
                    any.push_str(" || ");
                }
                any.push_str(&format!("{v} == {}", painless(o)?));
            }
            let inside = format!("({any})");
            if *negated { format!("(!{inside})") } else { inside }
        }
        Condition::IsNull { value, negated } => {
            let Expr::Field(f) = value else { return None };
            let empty = format!("(doc['{}'].size() == 0)", f.replace('\'', ""));
            if *negated { format!("(!{empty})") } else { empty }
        }
        _ => return None,
    })
}

/// A script an aggregation can be given, with the guard a missing field needs.
pub(crate) fn script_of(e: &Expr) -> Option<serde_json::Value> {
    let source = painless(e)?;
    Some(serde_json::json!({"source": source, "lang": "painless"}))
}
