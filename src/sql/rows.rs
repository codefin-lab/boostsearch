//! The answer, as a table.
//!
//! A search answers with documents or with buckets. Somebody who wrote SQL is
//! expecting rows and columns, so this is where one becomes the other.

use serde_json::{Value, json};

use super::ast::{Condition, Expr};
use super::plan::{Planned, Read};

/// A finished answer: what the columns are called, what type each holds, and
/// the rows themselves.
pub struct Table {
    pub columns: Vec<(String, String)>,
    /// the name a column was given with `AS`, where it was given one: the
    /// schema names both, as the reference's does
    pub aliases: Vec<Option<String>>,
    pub rows: Vec<Vec<Value>>,
    pub total: usize,
}

/// Read a search's answer into a table.
pub fn shape(planned: &Planned, answer: &Value) -> Table {
    let mut rows = if planned.grouped {
        from_buckets(planned, answer)
    } else {
        from_documents(planned, answer)
    };

    // `HAVING`, `ORDER BY` over an aggregate and `LIMIT` over groups all
    // happen here: the engine grouped the documents, and what to keep of the
    // groups is a question about the table
    if let Some(having) = &planned.having {
        // a `HAVING` names columns of the answer, so the row is turned into
        // an object of its own columns and asked
        rows.retain(|row| {
            let mut named = serde_json::Map::new();
            for (at, name) in planned.columns.iter().enumerate() {
                let value = row.get(at).cloned().unwrap_or(Value::Null);
                named.insert(name.clone(), value.clone());
                // `count(*) AS n` answers to `count(*)` as well as to `n`
                if let Some(Some(other)) = planned.also_called.get(at) {
                    named.insert(other.clone(), value);
                }
            }
            // a `HAVING` talks about the columns of the answer, so anything
            // it names that is one of them is that column's value rather than
            // something to work out again
            holds_named(having, &Value::Object(named))
        });
    }
    for (at, ascending) in planned.order_rows.iter().rev() {
        let at = *at;
        let ascending = *ascending;
        rows.sort_by(|a, b| {
            let (left, right) =
                (a.get(at).unwrap_or(&Value::Null), b.get(at).unwrap_or(&Value::Null));
            let order = order_of(left, right);
            if ascending { order } else { order.reverse() }
        });
    }
    // `DISTINCT` is about the rows together, which is only knowable here
    if planned.distinct {
        let mut seen = std::collections::HashSet::new();
        rows.retain(|row| seen.insert(Value::Array(row.clone()).to_string()));
        // The reference answers DISTINCT from a composite aggregation, so
        // the rows come back in key order; these came in document order.
        if planned.order_rows.is_empty() {
            rows.sort_by(|a, b| {
                a.iter()
                    .zip(b.iter())
                    .map(|(x, y)| order_of(x, y))
                    .find(|o| *o != std::cmp::Ordering::Equal)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
    }
    if planned.grouped || planned.distinct {
        if let Some(limit) = planned.limit {
            let from = planned.offset.min(rows.len());
            let to = (from + limit).min(rows.len());
            rows = rows[from..to].to_vec();
        } else if planned.offset > 0 {
            rows = rows[planned.offset.min(rows.len())..].to_vec();
        }
    }

    let total = rows.len();
    // a column that was only there to be sorted by goes no further: `top 3
    // customer` orders by a count the answer does not report
    let shown = planned.columns.len().saturating_sub(planned.hidden);
    let mut rows = rows;
    if planned.hidden > 0 {
        for row in rows.iter_mut() {
            row.truncate(shown);
        }
    }
    let rows = rows;
    let columns: Vec<(String, String)> = planned
        .columns
        .iter()
        .take(shown)
        .cloned()
        .collect::<Vec<_>>()
        .iter()
        .enumerate()
        .map(|(at, name)| {
            // counting answers a whole number, and the reference calls that
            // an `integer` however large the count is
            let counting = matches!(planned.reads.get(at), Some(Read::Count));
            let kind = if counting { "integer".to_string() } else { kind_of(&rows, at) };
            (name.clone(), kind)
        })
        .collect();
    // what a column was called with `AS`: the plan keeps the other way round
    // (the expression behind an alias), so a column that has one is a column
    // whose name is the alias
    let aliases = planned
        .also_called
        .iter()
        .take(shown)
        .enumerate()
        .map(|(at, behind)| behind.as_ref().map(|_| planned.columns[at].clone()))
        .collect();
    Table { columns, aliases, rows, total }
}

/// What type a column holds, judged by what is in it.
fn kind_of(rows: &[Vec<Value>], at: usize) -> String {
    let mut seen = "";
    for row in rows {
        let found = match row.get(at) {
            Some(Value::Number(n)) if n.is_i64() || n.is_u64() => "long",
            Some(Value::Number(_)) => "double",
            Some(Value::Bool(_)) => "boolean",
            Some(Value::String(_)) => "keyword",
            Some(Value::Array(_)) | Some(Value::Object(_)) => "object",
            _ => continue,
        };
        // a column holding both whole and fractional numbers is fractional
        seen = match (seen, found) {
            ("", other) => other,
            ("long", "double") | ("double", "long") => "double",
            (held, other) if held == other => held,
            _ => "keyword",
        };
    }
    if seen.is_empty() { "keyword".to_string() } else { seen.to_string() }
}

fn from_documents(planned: &Planned, answer: &Value) -> Vec<Vec<Value>> {
    let hits = answer.pointer("/hits/hits").and_then(|h| h.as_array()).cloned().unwrap_or_default();
    hits.iter()
        .skip(planned.offset)
        .map(|hit| {
            let source = hit.get("_source").cloned().unwrap_or_else(|| json!({}));
            planned
                .reads
                .iter()
                .map(|read| match read {
                    Read::All => source.clone(),
                    Read::Field(field) => at_path(&source, field),
                    Read::Constant(v) => v.clone(),
                    Read::Expr(e, _) => evaluate(e, &source),
                    // a document has no groups and no metrics
                    _ => Value::Null,
                })
                .collect()
        })
        .collect()
}

/// Walk the buckets, deepest first, and make a row of each leaf.
fn from_buckets(planned: &Planned, answer: &Value) -> Vec<Vec<Value>> {
    // `SELECT count(*) FROM t` needs no aggregation at all -- a search
    // already says how many documents it matched -- so the answer carries no
    // `aggregations` at all, and reading the rows out of them found none:
    // the query answered with no rows rather than with the count. Beside
    // another aggregate, or under a `GROUP BY`, there was an aggregation to
    // read and it worked, which is why only the plainest form of it was
    // wrong.
    let empty = Value::Object(Default::default());
    let aggs = answer.get("aggregations").unwrap_or(&empty);
    let depth = planned
        .reads
        .iter()
        .filter_map(|r| match r {
            Read::Key(at) => Some(*at + 1),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    // with no grouping at all there is one row, out of the metrics themselves
    if depth == 0 && aggs.get("g0").is_none() {
        // (the total is what `count(*)` reads)
        let total = answer.pointer("/hits/total/value").and_then(|v| v.as_u64()).unwrap_or(0);
        return vec![row_from(planned, &[], aggs, total)];
    }
    let mut rows = Vec::new();
    walk(planned, aggs, 0, &mut Vec::new(), &mut rows);
    rows
}

fn walk(
    planned: &Planned,
    here: &Value,
    depth: usize,
    keys: &mut Vec<Value>,
    rows: &mut Vec<Vec<Value>>,
) {
    let Some(buckets) = here.pointer(&format!("/g{depth}/buckets")).and_then(|b| b.as_array())
    else {
        // no group at this depth: this is where the metrics are
        let count = here.get("doc_count").and_then(|v| v.as_u64()).unwrap_or(0);
        rows.push(row_from(planned, keys, here, count));
        return;
    };
    for bucket in buckets {
        keys.push(bucket.get("key").cloned().unwrap_or(Value::Null));
        if bucket.get(format!("g{}", depth + 1)).is_some() {
            walk(planned, bucket, depth + 1, keys, rows);
        } else {
            let count = bucket.get("doc_count").and_then(|v| v.as_u64()).unwrap_or(0);
            rows.push(row_from(planned, keys, bucket, count));
        }
        keys.pop();
    }
}

fn row_from(planned: &Planned, keys: &[Value], holder: &Value, count: u64) -> Vec<Value> {
    planned
        .reads
        .iter()
        .map(|read| match read {
            Read::Key(at) => keys.get(*at).cloned().unwrap_or(Value::Null),
            Read::Count => json!(count),
            Read::Metric(name) => metric(holder, name),
            Read::Constant(v) => v.clone(),
            Read::Expr(e, named) => evaluate_over(e, holder, count, named),
            _ => Value::Null,
        })
        .collect()
}

/// A metric's value, by the name it was stored under.
///
/// A metric that holds several numbers -- `extended_stats`, `percentiles` --
/// is named with a dot and the one that is wanted.
fn metric(holder: &Value, name: &str) -> Value {
    let (outer, inner) = match name.split_once('.') {
        Some((outer, inner)) => (outer, Some(inner)),
        None => (name, None),
    };
    let Some(found) = holder.get(outer) else { return Value::Null };
    match inner {
        None => found.get("value").cloned().unwrap_or_else(|| {
            // `cardinality` and the rest answer with a value; a bucket of
            // several answers with something more shaped
            found.clone()
        }),
        Some(which) => found
            .get(which)
            .or_else(|| found.pointer(&format!("/values/{which}")))
            .cloned()
            .unwrap_or(Value::Null),
    }
}

/// A value out of a document, by its dotted path.
fn at_path(source: &Value, path: &str) -> Value {
    let mut here = source;
    for part in path.split('.') {
        match here.get(part) {
            Some(next) => here = next,
            None => return Value::Null,
        }
    }
    here.clone()
}

/// Work out an expression against one document.
pub fn evaluate(expr: &Expr, source: &Value) -> Value {
    match expr {
        Expr::Field(f) => at_path(source, f),
        Expr::Number(n) => json!(n),
        Expr::Text(t) => json!(t),
        Expr::Boolean(b) => json!(b),
        Expr::Null | Expr::Star => Value::Null,
        Expr::Negate(inner) => match as_number(&evaluate(inner, source)) {
            Some(n) => json!(-n),
            None => Value::Null,
        },
        Expr::Binary { op, left, right } => {
            arithmetic(op, &evaluate(left, source), &evaluate(right, source))
        }
        Expr::Call { name, args } => {
            let values: Vec<Value> = args.iter().map(|a| evaluate(a, source)).collect();
            function(name, &values)
        }
        Expr::Case { whens, otherwise } => {
            for (when, then) in whens {
                if holds(when, source) {
                    return evaluate(then, source);
                }
            }
            otherwise.as_ref().map(|e| evaluate(e, source)).unwrap_or(Value::Null)
        }
    }
}

/// The same, over a bucket rather than a document: an aggregate inside
/// arithmetic reads its own metric.
fn evaluate_over(
    expr: &Expr,
    holder: &Value,
    count: u64,
    named: &std::collections::BTreeMap<String, String>,
) -> Value {
    match expr {
        Expr::Call { name, .. } if name.eq_ignore_ascii_case("count") => json!(count),
        Expr::Call { name, args } if super::plan::is_aggregate(name) => {
            let _ = args;
            // the metric this very call was asked for. It used to be
            // whichever metric came first in the bucket, so `max(n) - min(n)`
            // read one of them twice and answered zero.
            match named.get(&expr.name()) {
                Some(m) => metric(holder, m),
                None => holder
                    .as_object()
                    .and_then(|o| {
                        o.iter()
                            .find(|(k, _)| k.starts_with('m'))
                            .and_then(|(_, v)| v.get("value").cloned())
                    })
                    .unwrap_or(Value::Null),
            }
        }
        Expr::Binary { op, left, right } => arithmetic(
            op,
            &evaluate_over(left, holder, count, named),
            &evaluate_over(right, holder, count, named),
        ),
        Expr::Negate(inner) => match as_number(&evaluate_over(inner, holder, count, named)) {
            Some(n) => json!(-n),
            None => Value::Null,
        },
        other => evaluate(other, &json!({})),
    }
}

/// Whether a condition holds of a row, where anything the row already has a
/// column for is read rather than recomputed.
pub fn holds_named(condition: &Condition, row: &Value) -> bool {
    match condition {
        Condition::And(a, b) => holds_named(a, row) && holds_named(b, row),
        Condition::Or(a, b) => holds_named(a, row) || holds_named(b, row),
        Condition::Not(inner) => !holds_named(inner, row),
        Condition::Compare { left, op, right } => {
            compare(&named_or_evaluated(left, row), op, &named_or_evaluated(right, row))
        }
        Condition::Between { value, low, high, negated } => {
            let (v, l, h) = (
                named_or_evaluated(value, row),
                named_or_evaluated(low, row),
                named_or_evaluated(high, row),
            );
            (compare(&v, ">=", &l) && compare(&v, "<=", &h)) != *negated
        }
        other => holds(other, row),
    }
}

/// A value the row already holds under this name, or one worked out.
fn named_or_evaluated(expr: &Expr, row: &Value) -> Value {
    if let Some(found) = row.get(expr.name()) {
        return found.clone();
    }
    evaluate(expr, row)
}

/// Whether a condition holds of a document, for the conditions that can be
/// answered without going back to the index -- which is what `CASE WHEN` and
/// `HAVING` need.
pub fn holds(condition: &Condition, source: &Value) -> bool {
    match condition {
        Condition::Always(b) => *b,
        Condition::And(a, b) => holds(a, source) && holds(b, source),
        Condition::Or(a, b) => holds(a, source) || holds(b, source),
        Condition::Not(inner) => !holds(inner, source),
        Condition::IsNull { value, negated } => {
            let found = evaluate(value, source);
            (found == Value::Null) != *negated
        }
        Condition::Compare { left, op, right } => {
            compare(&evaluate(left, source), op, &evaluate(right, source))
        }
        Condition::Between { value, low, high, negated } => {
            let (v, l, h) =
                (evaluate(value, source), evaluate(low, source), evaluate(high, source));
            (compare(&v, ">=", &l) && compare(&v, "<=", &h)) != *negated
        }
        Condition::In { value, options, negated } => {
            let found = evaluate(value, source);
            options.iter().any(|o| evaluate(o, source) == found) != *negated
        }
        Condition::Like { value, pattern, negated } => {
            let found = evaluate(value, source);
            let text = found.as_str().unwrap_or("");
            like(text, pattern) != *negated
        }
        // a full-text function cannot be answered from a row: it was already
        // asked of the index
        Condition::Search { .. } => true,
    }
}

fn like(text: &str, pattern: &str) -> bool {
    // `%` is any run, `_` is any one character
    let mut regex = String::from("^");
    for c in pattern.chars() {
        match c {
            '%' => regex.push_str(".*"),
            '_' => regex.push('.'),
            // anything a regular expression would read as syntax is written
            // as itself
            other => {
                if "\\.+*?()|[]{}^$".contains(other) {
                    regex.push('\\');
                }
                regex.push(other);
            }
        }
    }
    regex.push('$');
    regex::Regex::new(&regex).map(|r| r.is_match(text)).unwrap_or(false)
}

pub fn compare(left: &Value, op: &str, right: &Value) -> bool {
    let order = match (as_number(left), as_number(right)) {
        (Some(a), Some(b)) => a.partial_cmp(&b),
        _ => match (left.as_str(), right.as_str()) {
            (Some(a), Some(b)) => Some(a.cmp(b)),
            _ => None,
        },
    };
    let Some(order) = order else {
        return match op {
            "=" => left == right,
            "<>" | "!=" => left != right,
            _ => false,
        };
    };
    match op {
        "=" => order.is_eq(),
        "<>" | "!=" => !order.is_eq(),
        ">" => order.is_gt(),
        ">=" => order.is_ge(),
        "<" => order.is_lt(),
        "<=" => order.is_le(),
        _ => false,
    }
}

/// How two values sort against each other, with nothing last.
fn order_of(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (as_number(left), as_number(right)) {
        (Some(a), Some(b)) => a.total_cmp(&b),
        _ => match (left, right) {
            (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
            (Value::Null, _) => std::cmp::Ordering::Greater,
            (_, Value::Null) => std::cmp::Ordering::Less,
            _ => left.as_str().unwrap_or("").cmp(right.as_str().unwrap_or("")),
        },
    }
}

fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(f64::from(*b)),
        _ => None,
    }
}

fn arithmetic(op: &str, left: &Value, right: &Value) -> Value {
    if op == "||" {
        let text = format!(
            "{}{}",
            left.as_str().map(|s| s.to_string()).unwrap_or_else(|| text_of(left)),
            right.as_str().map(|s| s.to_string()).unwrap_or_else(|| text_of(right))
        );
        return json!(text);
    }
    let (Some(a), Some(b)) = (as_number(left), as_number(right)) else { return Value::Null };
    let found = match op {
        "+" => a + b,
        "-" => a - b,
        "*" => a * b,
        "/" => {
            if b == 0.0 {
                return Value::Null;
            }
            a / b
        }
        "%" => {
            if b == 0.0 {
                return Value::Null;
            }
            a % b
        }
        _ => return Value::Null,
    };
    // a double on either side makes a double, whole or not: `10.0 * 2`
    // is `20.0`, and answering `20` changed the type a client reads
    let fractional = |v: &Value| matches!(v, Value::Number(n) if n.is_f64());
    if fractional(left) || fractional(right) {
        return json!(found);
    }
    number(found)
}

/// A number, kept whole where it is whole.
fn number(found: f64) -> Value {
    if found.fract() == 0.0 && found.abs() < 9e15 { json!(found as i64) } else { json!(found) }
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The scalar functions, which work on one row at a time.
fn function(name: &str, args: &[Value]) -> Value {
    let one = |at: usize| args.get(at).cloned().unwrap_or(Value::Null);
    let num = |at: usize| args.get(at).and_then(as_number);
    let text = |at: usize| args.get(at).map(text_of).unwrap_or_default();
    match name.to_lowercase().as_str() {
        "abs" => num(0).map(|n| number(n.abs())).unwrap_or(Value::Null),
        "ceil" | "ceiling" => num(0).map(|n| number(n.ceil())).unwrap_or(Value::Null),
        "floor" => num(0).map(|n| number(n.floor())).unwrap_or(Value::Null),
        "round" => match (num(0), num(1)) {
            (Some(n), Some(places)) => {
                let scale = 10f64.powi(places as i32);
                json!((n * scale).round() / scale)
            }
            (Some(n), None) => number(n.round()),
            _ => Value::Null,
        },
        "sqrt" => num(0).filter(|n| *n >= 0.0).map(|n| json!(n.sqrt())).unwrap_or(Value::Null),
        "pow" | "power" => match (num(0), num(1)) {
            (Some(a), Some(b)) => number(a.powf(b)),
            _ => Value::Null,
        },
        "log" => num(0).filter(|n| *n > 0.0).map(|n| json!(n.ln())).unwrap_or(Value::Null),
        "log10" => num(0).filter(|n| *n > 0.0).map(|n| json!(n.log10())).unwrap_or(Value::Null),
        "exp" => num(0).map(|n| json!(n.exp())).unwrap_or(Value::Null),
        "upper" => json!(text(0).to_uppercase()),
        "lower" => json!(text(0).to_lowercase()),
        "trim" => json!(text(0).trim().to_string()),
        "ltrim" => json!(text(0).trim_start().to_string()),
        "rtrim" => json!(text(0).trim_end().to_string()),
        "length" => json!(text(0).chars().count()),
        "concat" => json!(args.iter().map(text_of).collect::<String>()),
        "substring" | "substr" => {
            let held: Vec<char> = text(0).chars().collect();
            // SQL counts from one
            let from = num(1).unwrap_or(1.0).max(1.0) as usize - 1;
            let take = num(2).map(|n| n as usize).unwrap_or(held.len());
            json!(held.iter().skip(from).take(take).collect::<String>())
        }
        "replace" => json!(text(0).replace(&text(1), &text(2))),
        "coalesce" => args.iter().find(|v| **v != Value::Null).cloned().unwrap_or(Value::Null),
        "if" => {
            let holds = matches!(one(0), Value::Bool(true))
                || as_number(&one(0)).map(|n| n != 0.0).unwrap_or(false);
            if holds { one(1) } else { one(2) }
        }
        "ifnull" => {
            if one(0) == Value::Null {
                one(1)
            } else {
                one(0)
            }
        }
        "cast" => one(0),
        _ => Value::Null,
    }
}
