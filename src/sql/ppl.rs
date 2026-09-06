//! PPL: the same questions, asked as a pipeline.
//!
//! `source=logs | where status = 500 | stats count() by region | sort - count`
//! says what a SELECT says, in the order somebody thinks it: start here,
//! narrow, group, sort, take. Each stage adds to the same statement a SELECT
//! would have built, so everything after this point is shared.

use super::ast::*;
use super::lexer::{Token, read};
use super::parser;

/// Read a pipeline into the statement it stands for.
pub fn parse(source: &str) -> Result<Select, String> {
    let mut select = Select {
        columns: vec![Column { expr: Expr::Star, alias: None }],
        from: String::new(),
        filter: None,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: 0,
        distinct: false,
    };
    // what `eval` has named, so that a later `fields` naming one of them
    // means the value rather than a field of the document that does not exist
    let mut evaluated: Vec<(String, Expr)> = Vec::new();
    // the stages, split on the pipe that separates them
    let stages: Vec<&str> = split_stages(source);
    for (at, stage) in stages.iter().enumerate() {
        let stage = stage.trim();
        if stage.is_empty() {
            continue;
        }
        let (command, rest) = command_of(stage);
        match command.to_lowercase().as_str() {
            "source" | "search" => {
                select.from = index_of(rest)?;
            }
            "where" => {
                let found = parse_condition(rest)?;
                select.filter = Some(match select.filter.take() {
                    Some(held) => Condition::And(Box::new(held), Box::new(found)),
                    None => found,
                });
            }
            "fields" => {
                select.columns = parse_columns(rest)?
                    .into_iter()
                    .map(|column| match &column.expr {
                        // a name `eval` gave to something is that something
                        Expr::Field(name) => match evaluated.iter().find(|(n, _)| n == name) {
                            Some((_, expr)) => Column {
                                expr: expr.clone(),
                                alias: Some(name.clone()),
                            },
                            None => column,
                        },
                        _ => column,
                    })
                    .collect();
            }
            "stats" => {
                let (columns, group_by) = parse_stats(rest)?;
                select.columns = columns;
                select.group_by = group_by;
            }
            "sort" => {
                select.order_by = parse_sort(rest)?;
            }
            "head" => {
                select.limit = Some(rest.trim().parse::<usize>().unwrap_or(10));
            }
            "dedup" => {
                select.distinct = true;
                if !rest.trim().is_empty() {
                    select.columns = parse_columns(rest)?;
                }
            }
            "eval" => {
                // `eval x = a + b` adds a column worked out from the row
                for piece in split_commas(rest) {
                    let (name, expression) = piece
                        .split_once('=')
                        .ok_or_else(|| format!("[eval] wants a name and a value: {piece}"))?;
                    let expr = parse_expr(expression)?;
                    // `fields` after an `eval` may name it, so the star that
                    // stands for everything makes room for it
                    if select.columns.iter().any(|c| matches!(c.expr, Expr::Star)) {
                        select.columns = vec![Column { expr: Expr::Star, alias: None }];
                    }
                    let name = name.trim().to_string();
                    evaluated.push((name.clone(), expr.clone()));
                    select.columns.push(Column { expr, alias: Some(name) });
                }
            }
            other if at == 0 => {
                // a pipeline that begins with a bare index name
                select.from = index_of(other)?;
            }
            other => return Err(format!("unsupported command [{other}]")),
        }
    }
    if select.from.is_empty() {
        return Err("a pipeline has to say where it reads from".to_string());
    }
    Ok(select)
}

/// Split on the pipes that separate stages, leaving alone any inside quotes.
fn split_stages(source: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = source.as_bytes();
    let (mut start, mut at, mut quoted) = (0usize, 0usize, false);
    while at < bytes.len() {
        match bytes[at] {
            b'\'' => quoted = !quoted,
            b'|' if !quoted => {
                out.push(&source[start..at]);
                start = at + 1;
            }
            _ => {}
        }
        at += 1;
    }
    out.push(&source[start..]);
    out
}

/// The first word of a stage, and everything after it.
///
/// A command ends where the letters do, so that `source=logs` and
/// `source = logs` are the same stage written two ways.
fn command_of(stage: &str) -> (&str, &str) {
    match stage.find(|c: char| !c.is_alphabetic()) {
        Some(at) => (&stage[..at], &stage[at..]),
        None => (stage, ""),
    }
}

/// `source=logs`, `source = logs`, or just `logs`.
fn index_of(rest: &str) -> Result<String, String> {
    let text = rest.trim().trim_start_matches('=').trim();
    let name = text.split_whitespace().next().unwrap_or_default();
    if name.is_empty() {
        return Err("no index named".to_string());
    }
    Ok(name.trim_start_matches('=').to_string())
}

fn split_commas(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let (mut start, mut at, mut depth, mut quoted) = (0usize, 0usize, 0i32, false);
    while at < bytes.len() {
        match bytes[at] {
            b'\'' => quoted = !quoted,
            b'(' if !quoted => depth += 1,
            b')' if !quoted => depth -= 1,
            b',' if !quoted && depth == 0 => {
                out.push(text[start..at].trim());
                start = at + 1;
            }
            _ => {}
        }
        at += 1;
    }
    let last = text[start..].trim();
    if !last.is_empty() {
        out.push(last);
    }
    out
}

/// A condition, read by the SQL parser so that both languages agree about
/// what `a > 1 AND b = 'x'` means.
fn parse_condition(text: &str) -> Result<Condition, String> {
    let select = parser::parse(&format!("SELECT * FROM _ WHERE {}", text.trim()))?;
    select.filter.ok_or_else(|| "empty condition".to_string())
}

fn parse_expr(text: &str) -> Result<Expr, String> {
    let select = parser::parse(&format!("SELECT {} FROM _", text.trim()))?;
    Ok(select.columns.into_iter().next().map(|c| c.expr).unwrap_or(Expr::Null))
}

fn parse_columns(text: &str) -> Result<Vec<Column>, String> {
    let named = text.trim();
    if named.is_empty() {
        return Ok(vec![Column { expr: Expr::Star, alias: None }]);
    }
    let select = parser::parse(&format!("SELECT {named} FROM _"))?;
    Ok(select.columns)
}

/// `stats count(), avg(price) by region, host`
fn parse_stats(text: &str) -> Result<(Vec<Column>, Vec<Expr>), String> {
    let lowered = text.to_lowercase();
    let (metrics, groups) = match lowered.find(" by ") {
        Some(at) => (&text[..at], Some(&text[at + 4..])),
        None => (text, None),
    };
    let mut columns = Vec::new();
    for piece in split_commas(metrics) {
        let select = parser::parse(&format!("SELECT {piece} FROM _"))?;
        columns.extend(select.columns);
    }
    let mut group_by = Vec::new();
    if let Some(groups) = groups {
        for piece in split_commas(groups) {
            let expr = parse_expr(piece)?;
            // a grouping key is a column of the answer as well
            columns.push(Column { expr: expr.clone(), alias: None });
            group_by.push(expr);
        }
    }
    Ok((columns, group_by))
}

/// `sort - count, + region`, where the sign says which way.
fn parse_sort(text: &str) -> Result<Vec<(Expr, bool)>, String> {
    let mut out = Vec::new();
    for piece in split_commas(text) {
        let piece = piece.trim();
        let (ascending, rest) = match piece.strip_prefix('-') {
            Some(rest) => (false, rest),
            None => (true, piece.strip_prefix('+').unwrap_or(piece)),
        };
        out.push((parse_expr(rest)?, ascending));
    }
    Ok(out)
}

/// The tokens of a pipeline, for anything that wants to look at it without
/// running it.
pub fn tokens(source: &str) -> Result<Vec<Token>, String> {
    read(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pipeline_says_where_it_reads_from() {
        let q = parse("source=logs").unwrap();
        assert_eq!(q.from, "logs");
        assert!(matches!(q.columns[0].expr, Expr::Star));
    }

    #[test]
    fn wheres_are_joined_rather_than_replaced() {
        let q = parse("source=logs | where a = 1 | where b = 2").unwrap();
        assert!(matches!(q.filter, Some(Condition::And(_, _))), "two wheres are both applied");
    }

    #[test]
    fn stats_becomes_a_group_by() {
        let q = parse("source=logs | stats count() by region").unwrap();
        assert_eq!(q.group_by.len(), 1);
        // the key is a column too, after the metric
        assert_eq!(q.columns.len(), 2);
        assert_eq!(q.columns[1].name(), "region");
    }

    #[test]
    fn sort_reads_the_sign() {
        let q = parse("source=logs | sort - price, + name").unwrap();
        assert_eq!(q.order_by.len(), 2);
        assert!(!q.order_by[0].1, "a minus means descending");
        assert!(q.order_by[1].1, "a plus means ascending");
    }

    #[test]
    fn head_is_a_limit() {
        assert_eq!(parse("source=logs | head 25").unwrap().limit, Some(25));
    }

    #[test]
    fn eval_adds_a_column() {
        let q = parse("source=logs | eval total = price * 2").unwrap();
        assert!(q.columns.iter().any(|c| c.name() == "total"));
    }

    #[test]
    fn a_pipe_inside_a_string_is_not_a_stage() {
        let q = parse("source=logs | where name = 'a|b'").unwrap();
        assert!(q.filter.is_some());
        assert_eq!(q.from, "logs");
    }

    #[test]
    fn a_pipeline_with_nowhere_to_read_from_is_refused() {
        assert!(parse("| where a = 1").is_err());
    }
}
