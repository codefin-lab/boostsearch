//! `_plugins/_sql` and `_plugins/_ppl`.

use super::*;
use crate::sql::plan::DEFAULT_ROWS;
use crate::sql::{parser, plan, ppl, rows};

/// How long a cursor's search context is kept waiting for the next page,
/// which is the plugin's `plugins.sql.cursor.keep_alive`.
const CURSOR_KEEP_ALIVE_MS: u64 = 60_000;

/// `POST _plugins/_sql`
pub async fn sql(State(store): State<Store>, Query(p): Query<Params>, body: String) -> Response {
    run(&store, &p, &body, false)
}

/// `GET _plugins/_ppl/_grammar` is not answered, and is the one read path on
/// this surface that stays a 501.
///
/// The reference answers it with the serialized ANTLR ATN of its two PPL
/// grammar files, a hash of those files, and the rule, token and channel
/// tables that go with them -- a quarter of a megabyte describing one
/// generated parser. Dashboards feeds it to an ANTLR runtime in the browser to
/// drive completion and to mark a query as invalid before it is sent.
///
/// That body is Apache-2.0 and could be carried here as data with attribution,
/// but carrying it would be a lie about this server rather than a description
/// of its interface, which is what separates it from the catalogues elsewhere
/// in this module's neighbours. The PPL here is a hand-written reader of about
/// a dozen stages (`src/sql/ppl.rs`); the reference's grammar has 262 parser
/// rules and 517 tokens. An editor driven by that ATN would complete `eventstats`,
/// `trendline`, `patterns`, `lookup`, `join` and the rest, and would pass a query
/// using them as valid, and then this node would refuse it -- the client would
/// be wrong before the request was ever made, which is the opposite of what the
/// endpoint is for. `grammarHash` makes it worse: it is a hash of grammar files
/// that are not in this repository, so answering with it asserts an identity
/// this parser does not have. And an ATN is a compiled artefact of an ANTLR
/// grammar; a recursive-descent reader has none to serialize and none could be
/// derived from it. So there is nothing truthful to answer, and the 501 stands
/// until the PPL here is generated from a grammar of its own.
///
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

/// `POST _plugins/_sql/close` -- let go of a cursor before it is walked out.
pub async fn close_cursor(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let parsed: Value = parse_body(&body).unwrap_or(json!({}));
    // the reference reads the body as a query and reports the field it could
    // not find, which is what a caller who sent no cursor is told
    let Some(text) = parsed.get("cursor").and_then(|v| v.as_str()) else {
        return backend_problem("JSONException", "JSONObject[\"query\"] not found.");
    };
    // a cursor already walked out is one there is nothing left to let go of,
    // and the reference says it succeeded either way
    if let Some(cursor) = Cursor::decode(text) {
        store.close_scroll(&cursor.context);
    }
    respond(&p, json!({"succeeded": true}))
}

/// Where a paged query has got to.
///
/// The cursor a client is handed is opaque -- the reference's is a compressed
/// blob of its own plan -- so what is in this one is nobody else's business.
/// It carries the query, where in the result the next page begins and how much
/// of the result is left, together with the search context the paging holds
/// open: a cursor sent back after the last page is then answered the way the
/// reference answers one, by saying the context is gone rather than by quietly
/// starting again.
struct Cursor {
    query: String,
    piped: bool,
    at: usize,
    left: usize,
    fetch: usize,
    context: String,
}

impl Cursor {
    /// The prefix the reference writes, kept so that a client which looks at
    /// the first two characters sees what it expects.
    const PREFIX: &'static str = "n:";

    fn encode(&self) -> String {
        use base64::Engine as _;
        let held = json!({
            "q": self.query, "p": self.piped, "at": self.at,
            "left": self.left, "fs": self.fetch, "id": self.context,
        });
        let text = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(held.to_string());
        format!("{}{text}", Self::PREFIX)
    }

    fn decode(text: &str) -> Option<Cursor> {
        use base64::Engine as _;
        let rest = text.strip_prefix(Self::PREFIX)?;
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(rest).ok()?;
        let held: Value = serde_json::from_slice(&raw).ok()?;
        Some(Cursor {
            query: held.get("q")?.as_str()?.to_string(),
            piped: held.get("p")?.as_bool()?,
            at: held.get("at")?.as_u64()? as usize,
            left: held.get("left")?.as_u64()? as usize,
            fetch: held.get("fs")?.as_u64()? as usize,
            context: held.get("id")?.as_str()?.to_string(),
        })
    }
}

/// The number a search context is named by in the message a missing one is
/// reported with. A context here is named by a token rather than numbered, so
/// the number the client is shown is made from the token.
fn context_number(id: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash % 100_000
}

/// A fault the plugin reports as its own rather than the query's.
fn backend_problem(kind: &str, details: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(json!({
            "error": {
                "reason": "There was internal problem at backend",
                "details": details,
                "type": kind,
            },
            "status": 500,
        })),
    )
        .into_response()
}

/// A search context that is no longer there, reported as the engine reports
/// it: the shards were asked, and the one holding the context had let it go.
fn context_missing(id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({
            "error": {
                "reason": "Error occurred in OpenSearch engine: all shards failed",
                "details": format!(
                    "Shard[0]: SearchContextMissingException[No search context found for id [{}]]\
                     \n\nFor more details, please send request for Json format to see the raw \
                     response from OpenSearch engine.",
                    context_number(id)
                ),
                "type": "SearchPhaseExecutionException",
            },
            "status": 404,
        })),
    )
        .into_response()
}

/// How many rows a page holds, where the caller asked for pages at all.
fn fetch_size_of(body: &Value) -> Result<usize, Response> {
    let Some(asked) = body.get("fetch_size") else { return Ok(0) };
    let size = asked.as_i64().or_else(|| asked.as_str().and_then(|s| s.parse().ok()));
    match size {
        Some(n) if n >= 0 => Ok(n as usize),
        _ => Err(failed(
            StatusCode::BAD_REQUEST,
            "IllegalArgumentException",
            "Fetch_size must be greater or equal to 0",
        )),
    }
}

/// Whether a query's rows can be handed out a page at a time.
///
/// Paging reads the result from an offset, so it only works where the order
/// of the rows is the index's to give. A query whose rows are grouped, made
/// distinct, filtered by `HAVING` or sorted over a column this server works
/// out is finished here, over the whole result: the reference does not page
/// those either, and answers them whole with no cursor.
fn pageable(planned: &plan::Planned) -> bool {
    !planned.grouped
        && !planned.distinct
        && planned.having.is_none()
        && planned.order_rows.is_empty()
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
    let parsed: Value = parse_body(body).unwrap_or(json!({}));
    // A request carrying a cursor is asking for the next page of a query it
    // has already sent, and the cursor is the whole of the request: the
    // reference reads nothing else in the body, not even a query beside it.
    let resumed = match parsed.get("cursor").and_then(|v| v.as_str()) {
        Some(text) => match Cursor::decode(text) {
            Some(c) => {
                // the paging holds a search context open, and a cursor sent
                // back after the last page names one that has been let go of
                if store.read_scroll(&c.context).is_none() {
                    return context_missing(&c.context);
                }
                Some(c)
            }
            None => {
                return backend_problem("UnsupportedOperationException", "Unsupported cursor");
            }
        },
        None => None,
    };
    // PPL has no paging: the reference reads `fetch_size` in a SQL body and
    // answers a piped query whole whatever is asked
    let fetch = match (&resumed, piped) {
        (Some(c), _) => c.fetch,
        (None, true) => 0,
        (None, false) => match fetch_size_of(&parsed) {
            Ok(n) => n,
            Err(r) => return r,
        },
    };
    let (text, piped) = match &resumed {
        Some(c) => (c.query.clone(), c.piped),
        None => (
            match query_of(body) {
                Ok(t) => t,
                Err(r) => return r,
            },
            piped,
        ),
    };
    let mut planned = match planned_of(&text, piped) {
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
    // Where the caller asked for pages, the search is asked for one page of
    // rows rather than the whole result, counting from where the last page
    // ended. `LIMIT` and `OFFSET` are the bounds of the result the pages are
    // cut out of: a query limited to ten rows hands out ten rows however
    // large `fetch_size` is.
    let paging = match (&resumed, fetch > 0 && pageable(&planned)) {
        (Some(c), _) => Some((c.at, c.left)),
        (None, true) => Some((planned.offset, planned.limit.unwrap_or(DEFAULT_ROWS))),
        (None, false) => None,
    };
    if let Some((at, left)) = paging {
        planned.body["from"] = json!(at);
        planned.body["size"] = json!(fetch.min(left));
        // the offset is spent on the search now, so the rows the answer holds
        // are the page itself
        planned.offset = 0;
    }
    // the search is coordinated from here like any other: the indices held on
    // other nodes are asked of those nodes, and the pages and aggregations
    // reduced over all of them
    let answer = match crate::search::run(store, &planned.index, &planned.body, &Params::new()) {
        Ok(out) => crate::search::envelope(out, &planned.body, &Params::new()),
        Err(r) => return r,
    };
    let table = typed_by_mapping(store, &planned, &targets, rows::shape(&planned, &answer));
    // The cursor for the page after this one, where there is one. A page that
    // came back short of what was asked for is the end of the result, and so
    // is one that used up what `LIMIT` allowed: the reference sends no cursor
    // with the last page, which is how a client knows to stop.
    let next = match paging {
        Some((at, left)) => {
            let want = fetch.min(left);
            let more = table.rows.len() == want && left > table.rows.len();
            match more {
                true => {
                    // the context is opened once the first page is known to
                    // have a successor, and carried through the rest of them
                    let context =
                        resumed.as_ref().map(|c| c.context.clone()).unwrap_or_else(|| {
                            store.open_scroll(
                                &planned.index,
                                &planned.body,
                                want,
                                None,
                                false,
                                CURSOR_KEEP_ALIVE_MS,
                                String::new(),
                            )
                        });
                    Some(
                        Cursor {
                            query: text.clone(),
                            piped,
                            at: at + table.rows.len(),
                            left: left - table.rows.len(),
                            fetch,
                            context,
                        }
                        .encode(),
                    )
                }
                false => {
                    if let Some(c) = &resumed {
                        store.close_scroll(&c.context);
                    }
                    None
                }
            }
        }
        None => None,
    };
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
            // the cursor comes last, after the status, as the reference
            // writes it
            if let Some(next) = next {
                answer["cursor"] = json!(next);
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

/// `GET _plugins/_query/_datasources` -- the catalogues a query may name.
///
/// A datasource is somewhere other than this cluster that a query can read
/// from: a Spark catalogue, an S3 table, a Prometheus server. A query here is
/// answered from the cluster's own indices and nothing else, so no datasource
/// is registered and the list is empty.
pub async fn datasources(Query(p): Query<Params>) -> Response {
    respond(&p, json!([]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_comes_back_as_it_went_out() {
        let held = Cursor {
            query: "SELECT a FROM t".to_string(),
            piped: false,
            at: 4,
            left: 196,
            fetch: 2,
            context: "velosearch-scroll-abc.6e6f6465".to_string(),
        };
        let text = held.encode();
        assert!(text.starts_with("n:"), "{text}");
        let read = Cursor::decode(&text).unwrap();
        assert_eq!(read.query, held.query);
        assert_eq!(read.piped, held.piped);
        assert_eq!((read.at, read.left, read.fetch), (4, 196, 2));
        assert_eq!(read.context, held.context);
    }

    #[test]
    fn a_cursor_that_is_not_one_is_not_read() {
        assert!(Cursor::decode("garbage").is_none());
        assert!(Cursor::decode("n:deadbeef").is_none());
        // the right prefix over something that is not a cursor's contents
        assert!(Cursor::decode("n:eyJhIjoxfQ").is_none());
    }

    #[test]
    fn a_fetch_size_is_a_count_or_a_refusal() {
        assert_eq!(fetch_size_of(&json!({})).ok(), Some(0));
        assert_eq!(fetch_size_of(&json!({"fetch_size": 0})).ok(), Some(0));
        assert_eq!(fetch_size_of(&json!({"fetch_size": 5})).ok(), Some(5));
        assert_eq!(fetch_size_of(&json!({"fetch_size": "5"})).ok(), Some(5));
        assert!(fetch_size_of(&json!({"fetch_size": -1})).is_err());
        assert!(fetch_size_of(&json!({"fetch_size": "many"})).is_err());
    }
}
