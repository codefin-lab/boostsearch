//! `_plugins/_sql` and `_plugins/_ppl`.

use super::*;
use crate::sql::{parser, plan, ppl, rows};

/// `POST _plugins/_sql`
pub async fn sql(State(store): State<Store>, Query(p): Query<Params>, body: String) -> Response {
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
pub async fn explain_sql(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    explain(&store, &p, &body, false)
}

pub async fn explain_ppl(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    explain(&store, &p, &body, true)
}

fn query_of(body: &str) -> Result<String, Response> {
    let parsed: Value = parse_body(body).unwrap_or(json!({}));
    // both languages name it the same way, which is what lets one handler
    // answer for both
    parsed.get("query").and_then(|v| v.as_str()).map(|s| s.to_string()).ok_or_else(|| {
        failed(StatusCode::BAD_REQUEST, "IllegalArgumentException", "[query] is missing")
    })
}

fn planned_of(text: &str, piped: bool) -> Result<plan::Planned, Response> {
    let select = if piped { ppl::parse(text) } else { parser::parse(text) }
        .map_err(|e| failed(StatusCode::BAD_REQUEST, "SyntaxAnalysisException", e))?;
    let planned = plan::plan(&select)
        .map_err(|e| failed(StatusCode::BAD_REQUEST, "SemanticAnalysisException", e))?;
    Ok(planned)
}

fn explain(store: &Store, p: &Params, body: &str, piped: bool) -> Response {
    let text = match query_of(body) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let planned = match planned_of(&text, piped) {
        Ok(p) => p,
        Err(r) => return r,
    };
    // the plan names the index the query would read, which is as much as
    // running it would tell a caller about what is there
    if let Some(why) = crate::security::item_refusal(
        store,
        &["indices:data/read/search"],
        &crate::security::layer::indices_for_expr(store, &planned.index),
    ) {
        return failed(StatusCode::FORBIDDEN, "SecurityException", why);
    }
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
    let text = match query_of(body) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let planned = match planned_of(&text, piped) {
        Ok(p) => p,
        Err(r) => return r,
    };
    // the indices the query reads, wherever in the cluster they are held: a
    // node holding no copy of one answers for it as the node holding it would
    let targets = crate::api::cluster_resolve(store, &planned.index);
    if targets.is_empty() {
        return failed(
            StatusCode::NOT_FOUND,
            "IndexNotFoundException",
            format!("no such index [{}]", planned.index),
        );
    }
    // A column no index maps is a mistake in the query, not a column of
    // nulls: the reference refuses it and names the symbol it could not
    // resolve. This answered rows of nulls, so a typo looked like an empty
    // field. `SELECT *` names nothing, an aggregate names what it counts, and
    // a name a document taught the index dynamically counts as mapped.
    if let Some(missing) = unresolved_column(store, &planned, &targets) {
        return failed(
            StatusCode::BAD_REQUEST,
            "SemanticCheckException",
            format!("can't resolve Symbol(namespace=FIELD_NAME, name={missing}) in type env"),
        );
    }
    // the index is named in the body, where the security layer cannot see
    // it, so it is judged here the way a bulk item is
    if let Some(why) = crate::security::item_refusal(store, &["indices:data/read/search"], &targets)
    {
        return failed(StatusCode::FORBIDDEN, "SecurityException", why);
    }
    // the search is coordinated from here like any other: the indices held on
    // other nodes are asked of those nodes, and the pages and aggregations
    // reduced over all of them
    let answer = match crate::search::run(store, &planned.index, &planned.body, &Params::new()) {
        Ok(out) => crate::search::envelope(out, &planned.body, &Params::new()),
        Err(r) => return r,
    };
    let table = typed_by_mapping(store, &planned, &targets, rows::shape(&planned, &answer));
    // the format decides the shape of the answer, not what is in it
    let format = p
        .get("format")
        .cloned()
        .or_else(|| {
            parse_body(body).ok().and_then(|b: Value| {
                b.get("format").and_then(|f| f.as_str()).map(|s| s.to_string())
            })
        })
        .unwrap_or_else(|| "jdbc".to_string());
    match format.as_str() {
        "csv" => text_answer(separated(&table, ','), "text/plain; charset=UTF-8"),
        "raw" => text_answer(separated(&table, '|'), "text/plain; charset=UTF-8"),
        "table" => text_answer(drawn(&table), "text/plain; charset=UTF-8"),
        "json" => respond(
            p,
            json!({"schema": schema(&table, piped), "datarows": table.rows,
                                     "total": table.total, "size": table.rows.len()}),
        ),
        _ => {
            let mut answer = json!({
                "schema": schema(&table, piped),
                "datarows": table.rows,
                "total": table.total,
                "size": table.rows.len(),
            });
            // SQL says `status` in its body and PPL does not
            if !piped {
                answer["status"] = json!(200);
            }
            respond(p, answer)
        }
    }
}

/// The `min`, `max` and `sum` of a whole-number field, as whole numbers.
///
/// The search answers every metric as a double, so `max(units)` over a
/// `long` came back `8.0` and typed `double`; the reference types it by the
/// field it read. The mapping says what that field is.
fn typed_by_mapping(
    store: &Store,
    planned: &plan::Planned,
    targets: &[String],
    mut table: rows::Table,
) -> rows::Table {
    fn find<'a>(node: &'a Value, name: &str) -> Option<&'a Value> {
        let o = node.as_object()?;
        if let Some(found) = o.get(name) {
            return Some(found);
        }
        o.values().find_map(|v| {
            v.get("aggs").or_else(|| v.get("aggregations")).and_then(|a| find(a, name))
        })
    }
    let Some(aggs) = planned.body.get("aggs") else { return table };
    let first = targets.first();
    for (at, read) in planned.reads.iter().enumerate() {
        let plan::Read::Metric(name) = read else { continue };
        let Some(def) = find(aggs, name) else { continue };
        let Some((kind, field)) = ["min", "max", "sum"].iter().find_map(|k| {
            def.get(*k).and_then(|d| d.get("field")).and_then(|f| f.as_str()).map(|f| (*k, f))
        }) else {
            continue;
        };
        let _ = kind;
        let mapped = first
            .and_then(|n| with_mapping(store, n, |m, _| m.type_of(field).map(|t| t.to_string())))
            .flatten();
        let Some(mapped) =
            mapped.filter(|t| matches!(t.as_str(), "long" | "integer" | "short" | "byte"))
        else {
            continue;
        };
        for row in table.rows.iter_mut() {
            if let Some(v) = row.get_mut(at)
                && let Some(n) = v.as_f64()
                && n.fract() == 0.0
            {
                *v = json!(n as i64);
            }
        }
        if let Some(col) = table.columns.get_mut(at) {
            col.1 = mapped;
        }
    }
    table
}

fn schema(table: &rows::Table, piped: bool) -> Vec<Value> {
    table
        .columns
        .iter()
        .zip(table.aliases.iter())
        // PPL names every text type `string`, where SQL keeps `keyword`, and
        // a count `int`, where SQL says `integer`
        .map(|((name, kind), alias)| {
            let kind = match (piped, kind.as_str()) {
                (true, "keyword") => "string",
                (true, "integer") => "int",
                (_, other) => other,
            };
            (name, kind, alias)
        })
        // PPL has no `AS`: a column it computes is named what it was called,
        // and the schema carries no alias beside it
        .map(|(name, kind, alias)| {
            if piped { (alias.as_ref().unwrap_or(name), kind, &None) } else { (name, kind, alias) }
        })
        .map(|(name, kind, alias)| match alias {
            // a column written `count(*) AS n` answers to both names, and the
            // schema says so: clients read the alias to label the column
            Some(alias) => json!({"name": name, "alias": alias, "type": kind}),
            None => json!({"name": name, "type": kind}),
        })
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
    let text_rows: Vec<Vec<String>> =
        table.rows.iter().map(|row| row.iter().map(|v| cell(v, '\0')).collect()).collect();
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

/// What an index maps, and the field types it has learned: this node's own
/// copy where it holds one, and the mapping the cluster published where it
/// holds none.
fn with_mapping<R>(
    store: &Store,
    index: &str,
    f: impl FnOnce(&crate::store::Mapping, &[(String, String)]) -> R,
) -> Option<R> {
    if let Some(st) = store.get(index) {
        let g = st.read();
        return Some(f(&g.mapping, &g.all_field_types()));
    }
    let published =
        crate::cluster::with_state(|s| s.indices.get(index).map(|m| m.mappings.clone()))?;
    let mapping = crate::store::Mapping::from_body(&published);
    let mut types: Vec<(String, String)> =
        mapping.types.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    types.sort();
    Some(f(&mapping, &types))
}

/// The first column the query names that no index behind it maps.
fn unresolved_column(
    store: &Store,
    planned: &crate::sql::plan::Planned,
    targets: &[String],
) -> Option<String> {
    if targets.is_empty() {
        return None;
    }
    let known = |name: &str| -> bool {
        // a metadata field is not in the mapping and is still a field
        if name.starts_with('_') || name == "*" {
            return true;
        }
        targets.iter().any(|n| {
            with_mapping(store, n, |mapping, types| {
                mapping.type_of(name).is_some()
                    || types.iter().any(|(f, _)| f == name)
                    || name
                        .rsplit_once('.')
                        .map(|(head, _)| mapping.type_of(head).is_some())
                        .unwrap_or(false)
            })
            .unwrap_or(false)
        })
    };
    planned.wanted_fields.iter().find(|f| !known(f)).cloned()
}

fn text_answer(text: String, kind: &str) -> Response {
    (StatusCode::OK, [(axum::http::header::CONTENT_TYPE, kind.to_string())], text).into_response()
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
