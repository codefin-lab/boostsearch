//! Turning a statement into a search.
//!
//! There is no second engine here. A `WHERE` becomes a query, a `GROUP BY`
//! becomes a terms aggregation, an aggregate column becomes a metric inside
//! it, and `ORDER BY` and `LIMIT` become a sort and a size. What comes back
//! is the ordinary answer, and `rows` turns it into a table.

use serde_json::{Value, json};

use super::ast::*;

/// A statement, as a search and the instructions for reading the answer.
pub struct Planned {
    pub index: String,
    pub body: Value,
    /// what each column is called, in the order asked for
    pub columns: Vec<String>,
    /// how each column is read out of the answer
    pub reads: Vec<Read>,
    /// whether the answer is buckets rather than documents
    pub grouped: bool,
    pub limit: Option<usize>,
    pub offset: usize,
    /// Everything that can only be decided once the rows exist: a `HAVING`
    /// is about a group, an `ORDER BY` over an aggregate is about a value the
    /// group produced, and `DISTINCT` is about the rows together. None of the
    /// three is a question the index can be asked.
    pub having: Option<Condition>,
    /// which column to sort by, and which way, where sorting happens here
    pub order_rows: Vec<(usize, bool)>,
    /// what each column may also be called: a column written `count(*) AS n`
    /// answers to both, and a `HAVING` may use either
    pub also_called: Vec<Option<String>>,
    pub distinct: bool,
}

/// Where one column's value comes from.
#[derive(Clone, Debug)]
pub enum Read {
    /// a field of the document, or of the bucket's key
    Field(String),
    /// the key of the group, by position
    Key(usize),
    /// a metric inside the bucket, by the name it was given
    Metric(String),
    /// how many documents the bucket holds
    Count,
    /// a value that does not depend on the document
    Constant(Value),
    /// worked out from the row once the rest of it is known
    Expr(Expr),
    /// every field the document has
    All,
}

pub fn plan(select: &Select) -> Result<Planned, String> {
    let grouped =
        !select.group_by.is_empty() || select.columns.iter().any(|c| c.expr.is_aggregate());
    if grouped { plan_grouped(select) } else { plan_rows(select) }
}

/// A query that asks for documents.
fn plan_rows(select: &Select) -> Result<Planned, String> {
    let mut body = json!({});
    if let Some(filter) = &select.filter {
        body["query"] = condition_to_query(filter)?;
    }
    let mut columns = Vec::new();
    let mut reads = Vec::new();
    let mut wanted: Vec<String> = Vec::new();
    for column in &select.columns {
        columns.push(column.name());
        match &column.expr {
            Expr::Star => {
                reads.push(Read::All);
            }
            Expr::Field(field) => {
                wanted.push(field.clone());
                reads.push(Read::Field(field.clone()));
            }
            Expr::Number(n) => reads.push(Read::Constant(json!(n))),
            Expr::Text(t) => reads.push(Read::Constant(json!(t))),
            Expr::Boolean(b) => reads.push(Read::Constant(json!(b))),
            Expr::Null => reads.push(Read::Constant(Value::Null)),
            other => {
                // anything worked out from the row needs the fields it is
                // worked out from
                collect_fields(other, &mut wanted);
                reads.push(Read::Expr(other.clone()));
            }
        }
    }
    // asking for the fields by name keeps the answer to what was asked for,
    // unless a `*` means all of it
    if !reads.iter().any(|r| matches!(r, Read::All)) && !wanted.is_empty() {
        body["_source"] = json!(wanted);
    }
    if !select.order_by.is_empty() {
        let mut sort = Vec::new();
        for (expr, ascending) in &select.order_by {
            let field = expr.field().ok_or_else(|| format!("cannot sort by [{}]", expr.name()))?;
            sort.push(json!({field: {"order": if *ascending { "asc" } else { "desc" }}}));
        }
        body["sort"] = Value::Array(sort);
    }
    // SQL counts from the top of the answer; a search counts from the top of
    // the index, so the offset is asked for as well as the limit
    let size = select.limit.unwrap_or(200);
    body["size"] = json!(size + select.offset);
    Ok(Planned {
        index: select.from.clone(),
        body,
        columns,
        reads,
        grouped: false,
        limit: select.limit,
        offset: select.offset,
        having: None,
        order_rows: Vec::new(),
        also_called: vec![None; select.columns.len()],
        distinct: select.distinct,
    })
}

/// A query that asks for groups.
fn plan_grouped(select: &Select) -> Result<Planned, String> {
    let mut body = json!({"size": 0});
    if let Some(filter) = &select.filter {
        body["query"] = condition_to_query(filter)?;
    }
    let mut columns = Vec::new();
    let mut reads = Vec::new();
    let mut metrics = serde_json::Map::new();

    // every grouping key, innermost last: a terms aggregation inside a terms
    // aggregation is how SQL's several keys are asked for
    let keys: Vec<String> = select
        .group_by
        .iter()
        .map(|e| {
            e.field()
                .map(|f| f.to_string())
                .ok_or_else(|| format!("cannot group by [{}]", e.name()))
        })
        .collect::<Result<_, _>>()?;

    for (position, column) in select.columns.iter().enumerate() {
        columns.push(column.name());
        match &column.expr {
            Expr::Field(field) => {
                let at = keys.iter().position(|k| k == field).ok_or_else(|| {
                    format!("[{field}] is not grouped by and is not an aggregate")
                })?;
                reads.push(Read::Key(at));
            }
            Expr::Call { name, args } if is_aggregate_name(name) => {
                let (metric, read) = metric_for(name, args, position)?;
                if let Some((metric_name, spec)) = metric {
                    metrics.insert(metric_name, spec);
                }
                reads.push(read);
            }
            Expr::Number(n) => reads.push(Read::Constant(json!(n))),
            Expr::Text(t) => reads.push(Read::Constant(json!(t))),
            other if other.is_aggregate() => {
                // an aggregate inside arithmetic: every aggregate under it is
                // asked for, and the arithmetic happens on the row
                let mut found = Vec::new();
                collect_aggregates(other, &mut found);
                for (at, call) in found.iter().enumerate() {
                    if let Expr::Call { name, args } = call {
                        let (metric, _) = metric_for(name, args, position * 100 + at)?;
                        if let Some((metric_name, spec)) = metric {
                            metrics.insert(metric_name, spec);
                        }
                    }
                }
                reads.push(Read::Expr(other.clone()));
            }
            other => {
                return Err(format!(
                    "[{}] is not grouped by and is not an aggregate",
                    other.name()
                ));
            }
        }
    }

    // the aggregations, built from the inside out
    let mut inner = Value::Object(metrics.clone());
    for (depth, key) in keys.iter().enumerate().rev() {
        let mut terms = json!({"terms": {"field": key, "size": 1000}});
        // the innermost group is the one that carries the metrics
        if !inner.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            terms["aggs"] = inner.clone();
        }
        inner = json!({format!("g{depth}"): terms});
    }
    if keys.is_empty() {
        // no grouping at all: the metrics stand on their own
        body["aggs"] = Value::Object(metrics);
    } else {
        body["aggs"] = inner;
    }
    // an `ORDER BY` over a group sorts the rows, since what it names is
    // something the group produced rather than something a document holds
    let mut order_rows = Vec::new();
    for (expr, ascending) in &select.order_by {
        let name = expr.name();
        let at = columns
            .iter()
            .position(|c| *c == name)
            .or_else(|| {
                // `ORDER BY m` where `m` was the alias of a column
                select.columns.iter().position(|c| c.name() == name)
            })
            .ok_or_else(|| {
                format!("cannot sort by [{name}], which is not a column of the answer")
            })?;
        order_rows.push((at, *ascending));
    }
    Ok(Planned {
        index: select.from.clone(),
        body,
        columns,
        reads,
        grouped: true,
        limit: select.limit,
        offset: select.offset,
        having: select.having.clone(),
        order_rows,
        // an aliased column keeps the name of what it was worked out from,
        // because that is what a `HAVING` is most likely to say
        also_called: select
            .columns
            .iter()
            .map(|c| c.alias.as_ref().map(|_| c.expr.name()))
            .collect(),
        distinct: select.distinct,
    })
}

pub use super::ast::is_aggregate_name;
pub use super::ast::is_aggregate_name as is_aggregate;

/// The aggregation one aggregate column needs, and how to read it back.
fn metric_for(
    name: &str,
    args: &[Expr],
    position: usize,
) -> Result<(Option<(String, Value)>, Read), String> {
    let lowered = name.to_lowercase();
    let field = args.first().and_then(|a| a.field()).map(|f| f.to_string());
    let metric_name = format!("m{position}");
    match lowered.as_str() {
        // counting documents needs no aggregation: a bucket knows how many it
        // holds, and a search knows how many it matched
        "count" if field.is_none() || matches!(args.first(), Some(Expr::Star)) => {
            Ok((None, Read::Count))
        }
        // counting a field counts the documents that have one
        "count" => {
            let field = field.ok_or("count needs a field")?;
            Ok((
                Some((metric_name.clone(), json!({"value_count": {"field": field}}))),
                Read::Metric(metric_name),
            ))
        }
        "count_distinct" => {
            let field = field.ok_or("count(distinct) needs a field")?;
            Ok((
                Some((metric_name.clone(), json!({"cardinality": {"field": field}}))),
                Read::Metric(metric_name),
            ))
        }
        "sum" | "avg" | "min" | "max" => {
            let field = field.ok_or_else(|| format!("{lowered} needs a field"))?;
            Ok((
                Some((metric_name.clone(), json!({lowered.clone(): {"field": field}}))),
                Read::Metric(metric_name),
            ))
        }
        "var_pop" | "var_samp" | "stddev_pop" | "stddev_samp" => {
            let field = field.ok_or_else(|| format!("{lowered} needs a field"))?;
            Ok((
                Some((metric_name.clone(), json!({"extended_stats": {"field": field}}))),
                Read::Metric(format!("{metric_name}.{lowered}")),
            ))
        }
        "percentile" | "percentile_approx" => {
            let field = field.ok_or("percentile needs a field")?;
            let which = args
                .get(1)
                .and_then(|a| match a {
                    Expr::Number(n) => Some(*n),
                    _ => None,
                })
                .unwrap_or(50.0);
            Ok((
                Some((
                    metric_name.clone(),
                    json!({"percentiles": {"field": field, "percents": [which]}}),
                )),
                Read::Metric(format!("{metric_name}.{which}")),
            ))
        }
        other => Err(format!("unsupported aggregate [{other}]")),
    }
}

fn collect_fields(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Field(f) => out.push(f.clone()),
        Expr::Call { args, .. } => args.iter().for_each(|a| collect_fields(a, out)),
        Expr::Binary { left, right, .. } => {
            collect_fields(left, out);
            collect_fields(right, out);
        }
        Expr::Negate(inner) => collect_fields(inner, out),
        Expr::Case { whens, otherwise } => {
            for (_, then) in whens {
                collect_fields(then, out);
            }
            if let Some(other) = otherwise {
                collect_fields(other, out);
            }
        }
        _ => {}
    }
}

fn collect_aggregates(expr: &Expr, out: &mut Vec<Expr>) {
    match expr {
        Expr::Call { name, .. } if is_aggregate_name(name) => out.push(expr.clone()),
        Expr::Call { args, .. } => args.iter().for_each(|a| collect_aggregates(a, out)),
        Expr::Binary { left, right, .. } => {
            collect_aggregates(left, out);
            collect_aggregates(right, out);
        }
        Expr::Negate(inner) => collect_aggregates(inner, out),
        _ => {}
    }
}

/// A `WHERE` as a query.
pub fn condition_to_query(condition: &Condition) -> Result<Value, String> {
    Ok(match condition {
        Condition::Always(true) => json!({"match_all": {}}),
        Condition::Always(false) => json!({"match_none": {}}),
        Condition::And(a, b) => {
            json!({"bool": {"filter": [condition_to_query(a)?, condition_to_query(b)?]}})
        }
        Condition::Or(a, b) => json!({"bool": {
            "should": [condition_to_query(a)?, condition_to_query(b)?],
            "minimum_should_match": 1
        }}),
        Condition::Not(inner) => json!({"bool": {"must_not": [condition_to_query(inner)?]}}),
        Condition::IsNull { value, negated } => {
            let field = value.field().ok_or("IS NULL needs a field")?;
            let exists = json!({"exists": {"field": field}});
            if *negated { exists } else { json!({"bool": {"must_not": [exists]}}) }
        }
        Condition::Between { value, low, high, negated } => {
            let field = value.field().ok_or("BETWEEN needs a field")?;
            let range = json!({"range": {field: {"gte": literal(low)?, "lte": literal(high)?}}});
            if *negated { json!({"bool": {"must_not": [range]}}) } else { range }
        }
        Condition::In { value, options, negated } => {
            let field = value.field().ok_or("IN needs a field")?;
            let values: Result<Vec<Value>, String> = options.iter().map(literal).collect();
            let terms = json!({"terms": {field: values?}});
            if *negated { json!({"bool": {"must_not": [terms]}}) } else { terms }
        }
        Condition::Like { value, pattern, negated } => {
            let field = value.field().ok_or("LIKE needs a field")?;
            // SQL's wildcards are `%` and `_`; a query's are `*` and `?`
            let translated: String = pattern
                .chars()
                .map(|c| match c {
                    '%' => '*',
                    '_' => '?',
                    other => other,
                })
                .collect();
            let wildcard = json!({"wildcard": {field: {"value": translated}}});
            if *negated { json!({"bool": {"must_not": [wildcard]}}) } else { wildcard }
        }
        Condition::Compare { left, op, right } => {
            let field = left
                .field()
                .or_else(|| right.field())
                .ok_or_else(|| format!("cannot compare [{}]", left.name()))?
                .to_string();
            // `1 < a` is `a > 1`: the field is whichever side it is on
            let (value, op) = if left.field().is_some() {
                (literal(right)?, op.clone())
            } else {
                (literal(left)?, mirror(op))
            };
            match op.as_str() {
                "=" => json!({"term": {field: {"value": value}}}),
                "<>" | "!=" => {
                    json!({"bool": {"must_not": [{"term": {field: {"value": value}}}]}})
                }
                ">" => json!({"range": {field: {"gt": value}}}),
                ">=" => json!({"range": {field: {"gte": value}}}),
                "<" => json!({"range": {field: {"lt": value}}}),
                "<=" => json!({"range": {field: {"lte": value}}}),
                other => return Err(format!("unsupported comparison [{other}]")),
            }
        }
        Condition::Search { name, args } => search_query(name, args)?,
    })
}

/// The comparison that means the same thing with its sides swapped.
fn mirror(op: &str) -> String {
    match op {
        ">" => "<",
        ">=" => "<=",
        "<" => ">",
        "<=" => ">=",
        other => other,
    }
    .to_string()
}

/// The full-text functions, which are what SQL over an index is for.
fn search_query(name: &str, args: &[Expr]) -> Result<Value, String> {
    let text = |at: usize| -> Result<String, String> {
        match args.get(at) {
            Some(Expr::Text(t)) => Ok(t.clone()),
            Some(Expr::Field(f)) => Ok(f.clone()),
            Some(Expr::Number(n)) => Ok(n.to_string()),
            _ => Err(format!("[{name}] wants a field and a phrase")),
        }
    };
    Ok(match name {
        "match" | "matchquery" | "match_query" => {
            json!({"match": {text(0)?: {"query": text(1)?}}})
        }
        "match_phrase" => json!({"match_phrase": {text(0)?: {"query": text(1)?}}}),
        "multi_match" => {
            let fields: Vec<String> = text(0)?.split(',').map(|f| f.trim().to_string()).collect();
            json!({"multi_match": {"query": text(1)?, "fields": fields}})
        }
        "query_string" => json!({"query_string": {"query": text(0)?}}),
        "simple_query_string" => json!({"simple_query_string": {"query": text(0)?}}),
        "wildcard_query" => json!({"wildcard": {text(0)?: {"value": text(1)?}}}),
        "regexp_query" => json!({"regexp": {text(0)?: {"value": text(1)?}}}),
        other => return Err(format!("unsupported function [{other}]")),
    })
}

/// A value as JSON, where it is one.
fn literal(expr: &Expr) -> Result<Value, String> {
    Ok(match expr {
        Expr::Number(n) => {
            // a whole number is written as one, so that a term query against
            // an integer field matches
            if n.fract() == 0.0 && n.abs() < 9e15 { json!(*n as i64) } else { json!(n) }
        }
        Expr::Text(t) => json!(t),
        Expr::Boolean(b) => json!(b),
        Expr::Null => Value::Null,
        Expr::Field(f) => json!(f),
        Expr::Negate(inner) => match literal(inner)? {
            Value::Number(n) => json!(-n.as_f64().unwrap_or(0.0)),
            other => other,
        },
        other => return Err(format!("[{}] is not a value", other.name())),
    })
}

#[cfg(test)]
mod tests {
    use super::super::parser::parse;
    use super::*;

    fn planned(sql: &str) -> Planned {
        plan(&parse(sql).unwrap()).unwrap()
    }

    #[test]
    fn a_where_becomes_a_query() {
        let p = planned("SELECT a FROM t WHERE a > 5");
        assert_eq!(p.body["query"], json!({"range": {"a": {"gt": 5}}}));
        assert!(!p.grouped);
    }

    #[test]
    fn a_field_on_the_right_is_still_the_field() {
        let p = planned("SELECT a FROM t WHERE 5 < a");
        assert_eq!(p.body["query"], json!({"range": {"a": {"gt": 5}}}));
    }

    #[test]
    fn like_speaks_sql_wildcards() {
        let p = planned("SELECT a FROM t WHERE name LIKE 'an%e_'");
        assert_eq!(p.body["query"], json!({"wildcard": {"name": {"value": "an*e?"}}}));
    }

    #[test]
    fn a_group_by_becomes_a_terms_aggregation() {
        let p = planned("SELECT region, count(*) FROM t GROUP BY region");
        assert!(p.grouped);
        assert_eq!(p.body["aggs"]["g0"]["terms"]["field"], json!("region"));
        assert_eq!(p.columns, vec!["region", "count(*)"]);
    }

    #[test]
    fn metrics_hang_inside_the_innermost_group() {
        let p = planned("SELECT a, b, avg(price) FROM t GROUP BY a, b");
        let inner = &p.body["aggs"]["g0"]["aggs"]["g1"]["aggs"];
        assert!(inner.as_object().unwrap().values().any(|m| m.get("avg").is_some()));
    }

    #[test]
    fn an_aggregate_without_a_group_stands_alone() {
        let p = planned("SELECT count(*), max(price) FROM t");
        assert!(p.grouped);
        assert_eq!(p.body["aggs"].as_object().unwrap().len(), 1, "count needs no aggregation");
    }

    #[test]
    fn a_column_that_is_neither_grouped_nor_aggregated_is_refused() {
        let bad = plan(&parse("SELECT a, count(*) FROM t GROUP BY b").unwrap());
        assert!(bad.is_err(), "selecting an ungrouped column should be an error");
    }

    #[test]
    fn an_offset_asks_for_enough_to_skip() {
        let p = planned("SELECT a FROM t LIMIT 10 OFFSET 20");
        assert_eq!(p.body["size"], json!(30));
        assert_eq!((p.limit, p.offset), (Some(10), 20));
    }
}
