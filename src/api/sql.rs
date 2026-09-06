//! `_plugins/_sql` and `_plugins/_ppl`.

use super::*;
use crate::sql::{parser, plan, ppl, rows};

/// `POST _plugins/_sql`
pub async fn sql(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    run(&store, &p, &body, false)
}

/// `POST _plugins/_ppl`
pub async fn pipeline(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    run(&store, &p, &body, true)
}

/// `POST _plugins/_sql/_explain` and its PPL twin -- the search a query
/// would run, without running it.
pub async fn explain_sql(Query(p): Query<Params>, body: String) -> Response {
    explain(&p, &body, false)
}

pub async fn explain_ppl(Query(p): Query<Params>, body: String) -> Response {
    explain(&p, &body, true)
}

fn query_of(body: &str, piped: bool) -> Result<String, Response> {
    let parsed: Value = parse_body(body).unwrap_or(json!({}));
    let key = if piped { "query" } else { "query" };
    parsed
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| failed(StatusCode::BAD_REQUEST, "IllegalArgumentException", "[query] is missing"))
}

fn planned_of(text: &str, piped: bool) -> Result<plan::Planned, Response> {
    let select = if piped { ppl::parse(text) } else { parser::parse(text) }
        .map_err(|e| failed(StatusCode::BAD_REQUEST, "SyntaxAnalysisException", e))?;
    let planned = plan::plan(&select)
        .map_err(|e| failed(StatusCode::BAD_REQUEST, "SemanticAnalysisException", e))?;
    Ok(planned)
}

fn explain(p: &Params, body: &str, piped: bool) -> Response {
    let text = match query_of(body, piped) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let planned = match planned_of(&text, piped) {
        Ok(p) => p,
        Err(r) => return r,
    };
    // what the engine will actually be asked, which is the only honest answer
    // to "explain": not a description of a plan, the plan itself
    respond(
        p,
        json!({
            "root": {
                "name": "SearchRequest",
                "description": {
                    "request": format!("SearchRequest(indices=[{}], source={})", planned.index, planned.body),
                },
                "children": [],
            }
        }),
    )
}

fn run(store: &Store, p: &Params, body: &str, piped: bool) -> Response {
    let text = match query_of(body, piped) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let planned = match planned_of(&text, piped) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if store.resolve(&planned.index).is_empty() {
        return failed(
            StatusCode::NOT_FOUND,
            "IndexNotFoundException",
            format!("no such index [{}]", planned.index),
        );
    }
    let answer = match crate::search::run(store, &planned.index, &planned.body, &Params::new()) {
        Ok(out) => crate::search::envelope(out, &planned.body, &Params::new()),
        Err(r) => return r,
    };
    let table = rows::shape(&planned, &answer);
    // the format decides the shape of the answer, not what is in it
    let format = p
        .get("format")
        .cloned()
        .or_else(|| parse_body(body).ok().and_then(|b: Value| {
            b.get("format").and_then(|f| f.as_str()).map(|s| s.to_string())
        }))
        .unwrap_or_else(|| "jdbc".to_string());
    match format.as_str() {
        "csv" => text_answer(separated(&table, ','), "text/plain; charset=UTF-8"),
        "raw" => text_answer(separated(&table, '|'), "text/plain; charset=UTF-8"),
        "table" => text_answer(drawn(&table), "text/plain; charset=UTF-8"),
        "json" => respond(p, json!({"schema": schema(&table), "datarows": table.rows,
                                     "total": table.total, "size": table.rows.len()})),
        _ => respond(
            p,
            json!({
                "schema": schema(&table),
                "datarows": table.rows,
                "total": table.total,
                "size": table.rows.len(),
                "status": 200,
            }),
        ),
    }
}

fn schema(table: &rows::Table) -> Vec<Value> {
    table
        .columns
        .iter()
        .map(|(name, kind)| json!({"name": name, "type": kind}))
        .collect()
}

/// A table as lines of values, which is what `csv` and `raw` are.
fn separated(table: &rows::Table, by: char) -> String {
    let mut out = String::new();
    let names: Vec<String> = table.columns.iter().map(|(n, _)| n.clone()).collect();
    out.push_str(&names.join(&by.to_string()));
    out.push('\n');
    for row in &table.rows {
        let cells: Vec<String> = row.iter().map(|v| cell(v, by)).collect();
        out.push_str(&cells.join(&by.to_string()));
        out.push('\n');
    }
    out
}

/// One value, written so that reading the line back gives it again.
fn cell(value: &Value, by: char) -> String {
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    if by == ',' && (text.contains(',') || text.contains('"') || text.contains('\n')) {
        return format!("\"{}\"", text.replace('"', "\"\""));
    }
    text
}

/// A table drawn with lines, for somebody reading it rather than parsing it.
fn drawn(table: &rows::Table) -> String {
    let names: Vec<String> = table.columns.iter().map(|(n, _)| n.clone()).collect();
    let mut widths: Vec<usize> = names.iter().map(|n| n.chars().count()).collect();
    let text_rows: Vec<Vec<String>> = table
        .rows
        .iter()
        .map(|row| row.iter().map(|v| cell(v, '\0')).collect())
        .collect();
    for row in &text_rows {
        for (at, value) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(at) {
                *width = (*width).max(value.chars().count());
            }
        }
    }
    let line = |left: &str, middle: &str, right: &str| {
        let mut out = String::from(left);
        for (at, width) in widths.iter().enumerate() {
            out.push_str(&"-".repeat(width + 2));
            out.push_str(if at + 1 == widths.len() { right } else { middle });
        }
        out.push('\n');
        out
    };
    let write = |cells: &[String]| {
        let mut out = String::from("|");
        for (at, value) in cells.iter().enumerate() {
            let width = widths.get(at).copied().unwrap_or(0);
            out.push_str(&format!(" {value:<width$} |"));
        }
        out.push('\n');
        out
    };
    let mut out = line("+", "+", "+");
    out.push_str(&write(&names));
    out.push_str(&line("+", "+", "+"));
    for row in &text_rows {
        out.push_str(&write(row));
    }
    out.push_str(&line("+", "+", "+"));
    out
}

fn text_answer(text: String, kind: &str) -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, kind.to_string())],
        text,
    )
        .into_response()
}

/// An error, in the shape the SQL plugin reports one.
fn failed(status: StatusCode, kind: &str, reason: impl std::fmt::Display) -> Response {
    (
        status,
        axum::Json(json!({
            "error": {
                "reason": "Invalid SQL query",
                "details": reason.to_string(),
                "type": kind,
            },
            "status": status.as_u16(),
        })),
    )
        .into_response()
}

/// `GET _plugins/_sql/stats`
pub async fn stats(Query(p): Query<Params>) -> Response {
    respond(
        &p,
        json!({
            "failed_request_count_cus": 0,
            "failed_request_count_cuss": 0,
            "failed_request_count_syserr": 0,
            "circuit_breaker": 0,
            "request_total": 0,
            "request_count": 0,
            "failed_request_count_cb": 0,
        }),
    )
}
