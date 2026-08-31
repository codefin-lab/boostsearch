//! Columns into text: which are shown, how wide, and under what heading.

use super::*;

/// Columns the endpoint can show, for `?help`.
pub(crate) fn cat_help(columns: &[&str]) -> Response {
    let mut out = String::new();
    for c in columns {
        out.push_str(c);
        out.push_str(" | | \n");
    }
    out.into_response()
}

pub(crate) fn cat_render(rows: Vec<Vec<(&str, String)>>, p: &Params) -> Response {
    let cols: Vec<&str> =
        rows.first().map(|r| r.iter().map(|(k, _)| *k).collect()).unwrap_or_default();
    cat_render_cols(&cols, rows, p)
}

/// Columns are passed separately so `?help` and the `?v` header still work on an
/// endpoint that currently has no rows to show.
/// `_cat` columns answer to their full name, to the part after the last dot,
/// and to a leading-letter abbreviation -- `a` for `alias`, `rs` for
/// `routing.search`.
/// The short names `_cat` gives columns whose own name is nothing like them.
pub(crate) fn cat_column_alias(column: &str, asked: &str) -> bool {
    const ALIASES: &[(&str, &str)] = &[
        // `i` is the index where there is one and the address otherwise; the
        // row's own column order settles which, since a table with an index
        // column lists it first
        ("index", "i"),
        ("host", "h"),
        ("ip", "i"),
        ("port", "po"),
        ("node_name", "nn"),
        ("diskAvail", "disk"),
        ("diskAvail", "d"),
        ("diskTotal", "dt"),
        ("diskUsed", "du"),
        ("diskUsedPercent", "dup"),
        // how a shard was recovered, which cat writes in short form
        ("shard", "s"),
        ("time", "t"),
        ("type", "ty"),
        ("stage", "st"),
        ("source_host", "shost"),
        ("target_host", "thost"),
        ("source_node", "snode"),
        ("target_node", "tnode"),
        ("repository", "rep"),
        ("snapshot", "snap"),
        ("files", "f"),
        ("files_recovered", "fr"),
        ("files_percent", "fp"),
        ("files_total", "tf"),
        ("bytes", "b"),
        ("bytes_recovered", "br"),
        ("bytes_percent", "bp"),
        ("bytes_total", "tb"),
        ("translog_ops", "to"),
        ("translog_ops_recovered", "tor"),
        ("translog_ops_percent", "top"),
    ];
    ALIASES.iter().any(|(col, short)| *col == column && *short == asked)
}

pub(crate) fn cat_column_matches(column: &str, asked: &str) -> bool {
    if column == asked {
        return true;
    }
    let tail = column.rsplit('.').next().unwrap_or(column);
    if tail == asked {
        return true;
    }
    // the short names `_cat` accepts for columns whose own name is nothing
    // like them
    if cat_column_alias(column, asked) {
        return true;
    }
    let initials: String = column.split('.').filter_map(|p| p.chars().next()).collect();
    initials == asked
        || column.starts_with(asked) && !asked.is_empty() && column.len() > asked.len()
}

pub(crate) fn cat_render_cols(
    columns: &[&str],
    rows: Vec<Vec<(&str, String)>>,
    p: &Params,
) -> Response {
    if p.contains_key("help") {
        return cat_help(columns);
    }
    // `s=` orders the rows by named columns, each optionally `:desc`. A column
    // may be named by any of its aliases, which is how `s=index,a:desc` asks
    // for alias descending within index.
    let mut rows = rows;
    if let Some(spec) = p.get("s").filter(|s| !s.is_empty()) {
        let keys: Vec<(String, bool)> = spec
            .split(',')
            .map(|k| {
                let k = k.trim();
                match k.split_once(':') {
                    Some((name, dir)) => (name.to_string(), dir.eq_ignore_ascii_case("desc")),
                    None => (k.to_string(), false),
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            for (name, desc) in &keys {
                let pick = |r: &Vec<(&str, String)>| {
                    r.iter()
                        .find(|(k, _)| cat_column_matches(k, name))
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default()
                };
                let ord = pick(a).cmp(&pick(b));
                let ord = if *desc { ord.reverse() } else { ord };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
    }
    // `h=` picks and orders the columns
    let rows: Vec<Vec<(&str, String)>> = match p.get("h") {
        Some(spec) if !spec.is_empty() => {
            let want: Vec<&str> = spec.split(',').map(|s| s.trim()).collect();
            rows.into_iter()
                .map(|r| {
                    want.iter()
                        .flat_map(|w| {
                            // a name with a `*` in it stands for every column
                            // it fits, in the order the row holds them
                            if w.contains('*') {
                                return r
                                    .iter()
                                    .filter(|(k, _)| crate::store::glob_match(w, k))
                                    .map(|(k, v)| (*k, v.clone()))
                                    .collect::<Vec<_>>();
                            }
                            // a column answers to its name or to one of the
                            // short forms `_cat` accepts, and is headed by the
                            // name it was asked for
                            // an exact name wins, then a short form `_cat`
                            // names outright, and only then a prefix -- or
                            // `i` would find `id` before `ip`
                            r.iter()
                                .find(|(k, _)| k == w)
                                .or_else(|| r.iter().find(|(k, _)| cat_column_alias(k, w)))
                                .or_else(|| r.iter().find(|(k, _)| cat_column_matches(k, w)))
                                .map(|(_, v)| (*w, v.clone()))
                                .into_iter()
                                .collect::<Vec<_>>()
                        })
                        .collect()
                })
                .collect()
        }
        _ => rows,
    };
    if p.get("format").map(|f| f == "json").unwrap_or(false) {
        let arr: Vec<Value> = rows
            .iter()
            .map(|r| Value::Object(r.iter().map(|(k, v)| (k.to_string(), json!(v))).collect()))
            .collect();
        return axum::Json(arr).into_response();
    }
    // plain text: the format `cat` is named for. Cells are padded to the width
    // of their column so the values line up down the page.
    let show_head = p.contains_key("v") && p.get("v").map(|v| v != "false").unwrap_or(true);
    // with no rows to read the columns off, `h=` still says which were asked
    // for, and in what order
    let asked: Vec<&str> = p
        .get("h")
        .filter(|s| !s.is_empty())
        .map(|spec| spec.split(',').map(|s| s.trim()).collect())
        .unwrap_or_default();
    let head: Vec<&str> = match rows.first() {
        Some(r) => r.iter().map(|(k, _)| *k).collect(),
        None if !asked.is_empty() => asked,
        None => columns.to_vec(),
    };
    let mut widths: Vec<usize> =
        if show_head { head.iter().map(|h| h.len()).collect() } else { vec![0; head.len()] };
    for r in &rows {
        for (i, (_, v)) in r.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(v.len());
            }
        }
    }
    // alignment is a property of the column, not of what lands in it: the
    // ones an allocation is measured in line up on their right edge
    const RIGHT_SUFFIX: &[&str] =
        &[".indices", ".used", ".avail", ".total", ".percent", ".current", ".max"];
    let numeric: Vec<bool> = head
        .iter()
        .map(|h| *h == "shards" || RIGHT_SUFFIX.iter().any(|sfx| h.ends_with(sfx)))
        .collect();
    let line = |cells: Vec<&str>| {
        let mut s = String::new();
        for (i, c) in cells.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            let width = widths.get(i).copied().unwrap_or(0);
            let right = numeric.get(i).copied().unwrap_or(false);
            if right {
                for _ in c.len()..width {
                    s.push(' ');
                }
            }
            s.push_str(c);
            // the last cell needs no padding: nothing follows it to line up
            if !right && i + 1 < cells.len() {
                for _ in c.len()..width {
                    s.push(' ');
                }
            }
        }
        s.push('\n');
        s
    };
    let mut out = String::new();
    if show_head && !head.is_empty() {
        out.push_str(&line(head.clone()));
    }
    for r in &rows {
        out.push_str(&line(r.iter().map(|(_, v)| v.as_str()).collect()));
    }
    out.into_response()
}

/// An endpoint with no rows on a single node still has to answer `?help`.
pub(crate) fn cat_named(columns: &[&str], p: &Params) -> Response {
    cat_render_cols(columns, Vec::new(), p)
}

/// Some `_cat` columns exist for `h=` to ask for but are not in the table a
/// bare request returns. Drop those unless the caller named columns.
pub(crate) fn cat_only_default<'a>(
    rows: Vec<Vec<(&'a str, String)>>,
    defaults: &[&str],
    p: &Params,
) -> Vec<Vec<(&'a str, String)>> {
    if p.get("h").map(|h| !h.is_empty()).unwrap_or(false) {
        return rows;
    }
    rows.into_iter()
        .map(|r| r.into_iter().filter(|(k, _)| defaults.contains(k)).collect())
        .collect()
}
