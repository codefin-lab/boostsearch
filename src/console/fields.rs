//! The fields behind an index pattern.
//!
//! An index pattern is a name with a wildcard in it, and what the pages
//! want to know about it is which fields the indices it matches have between
//! them, of what type, and whether each can be searched, aggregated and read
//! from doc values. The engine answers that per index and per type
//! (`_field_caps`); this folds it into one list per pattern, the way the
//! server being replaced does (`index_patterns/fetcher`), because the front
//! end reads that list for its type names -- `string` rather than `keyword`,
//! `number` rather than `long`, `conflict` where two indices disagree.

use serde_json::{Map, Value, json};

use super::engine::{Engine, Failed};

/// What the engine calls a type, and what the page calls it.
///
/// This is the table the server being replaced keeps
/// (`osd_field_types_factory`), and a type it does not know is `unknown`.
fn page_type(engine_type: &str) -> &'static str {
    match engine_type {
        "string" | "text" | "match_only_text" | "wildcard" | "keyword" | "_type" | "_id" => {
            "string"
        }
        "float" | "half_float" | "scaled_float" | "double" | "integer" | "long"
        | "unsigned_long" | "short" | "byte" | "token_count" => "number",
        "date" | "date_nanos" => "date",
        "ip" => "ip",
        "boolean" => "boolean",
        "object" => "object",
        "nested" => "nested",
        "geo_point" => "geo_point",
        "geo_shape" => "geo_shape",
        "attachment" => "attachment",
        "murmur3" => "murmur3",
        "_source" => "_source",
        "histogram" => "histogram",
        _ => "unknown",
    }
}

/// Whether a field's values are read from doc values rather than the
/// source: anything aggregatable but text, shapes and the meta fields.
fn read_from_doc_values(aggregatable: bool, engine_type: &str) -> bool {
    aggregatable && !matches!(engine_type, "text" | "geo_shape") && !engine_type.starts_with('_')
}

/// The fields of everything a pattern matches, as one sorted list.
///
/// `meta_fields` are names the page wants an entry for whether or not the
/// engine has one -- `_id`, `_source` -- and get a plain string entry when
/// it does not.
pub fn for_wildcard(
    engine: &Engine,
    pattern: &str,
    meta_fields: &[String],
) -> Result<Vec<Value>, Failed> {
    let caps = field_caps(engine, pattern)?;
    Ok(fold(&caps, meta_fields))
}

/// The fields of the last `look_back` indices a time pattern matches.
///
/// `[logs-]YYYY.MM.DD` matches `logs-2017.01.02`: the bracketed part is
/// literal and the rest is a date. The indices that parse as one are put
/// newest first, and the newest `look_back` of them are what is asked
/// about.
pub fn for_time_pattern(
    engine: &Engine,
    pattern: &str,
    look_back: usize,
    meta_fields: &[String],
) -> Result<Vec<Value>, Failed> {
    let matches = resolve_time_pattern(engine, pattern)?;
    let indices: Vec<&str> = matches.iter().take(look_back).map(String::as_str).collect();
    if indices.is_empty() {
        return Err(no_matching_indices(pattern));
    }
    let caps = field_caps(engine, &indices.join(","))?;
    Ok(fold(&caps, meta_fields))
}

/// The refusal the server being replaced gives when nothing matches: a 404
/// whose attributes name the code, which the index-pattern page looks for.
fn no_matching_indices(pattern: &str) -> Failed {
    Failed::of(404, format!("No indices match pattern \"{pattern}\""))
}

fn field_caps(engine: &Engine, indices: &str) -> Result<Value, Failed> {
    let path = format!(
        "/{}/_field_caps?fields=*&ignore_unavailable=true&allow_no_indices=false",
        percent_encoding::utf8_percent_encode(indices, percent_encoding::NON_ALPHANUMERIC)
            .to_string()
            .replace("%2C", ",")
            .replace("%2A", "*")
    );
    let found = engine.call("GET", &path, None).map_err(|e| Failed::of(404, e.message))?;
    match found.pointer("/error/type").and_then(|v| v.as_str()) {
        Some("index_not_found_exception") => Err(no_matching_indices(indices)),
        Some(other) => Err(Failed::of(404, other.to_string())),
        None => Ok(found),
    }
}

/// The engine's answer -- one entry per field per type -- folded into one
/// entry per field.
fn fold(caps: &Value, meta_fields: &[String]) -> Vec<Value> {
    let by_name = caps.get("fields").and_then(|v| v.as_object()).cloned().unwrap_or_default();
    let mut read: Vec<Map<String, Value>> = Vec::new();
    for (name, by_type) in &by_name {
        let Some(by_type) = by_type.as_object() else { continue };
        let types: Vec<&String> = by_type.keys().collect();
        let has = |key: &str| {
            by_type.values().any(|cap| {
                cap.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
                    || cap
                        .get(format!("non_{key}_indices"))
                        .and_then(|v| v.as_array())
                        .is_some_and(|a| !a.is_empty())
            })
        };
        let searchable = has("searchable");
        let aggregatable = has("aggregatable");
        let mut page_types: Vec<&str> = types.iter().map(|t| page_type(t)).collect();
        page_types.dedup();
        let mut page_types_unique = page_types.clone();
        page_types_unique.sort_unstable();
        page_types_unique.dedup();
        let mut field = Map::new();
        field.insert("name".into(), json!(name));
        if page_types_unique.len() > 1 {
            field.insert("type".into(), json!("conflict"));
            field.insert("esTypes".into(), json!(types));
            field.insert("searchable".into(), json!(searchable));
            field.insert("aggregatable".into(), json!(aggregatable));
            field.insert("readFromDocValues".into(), json!(false));
            let mut descriptions = Map::new();
            for t in &types {
                descriptions.insert(
                    (*t).clone(),
                    by_type.get(*t).and_then(|c| c.get("indices")).cloned().unwrap_or(Value::Null),
                );
            }
            field.insert("conflictDescriptions".into(), Value::Object(descriptions));
        } else {
            let engine_type = types.first().map(|s| s.as_str()).unwrap_or("");
            field.insert("type".into(), json!(page_type(engine_type)));
            field.insert("esTypes".into(), json!(types));
            field.insert("searchable".into(), json!(searchable));
            field.insert("aggregatable".into(), json!(aggregatable));
            field.insert(
                "readFromDocValues".into(),
                json!(read_from_doc_values(aggregatable, engine_type)),
            );
        }
        read.push(field);
    }
    // a field with a dot in its name is under another: a multi-field of a
    // string, or a field of a nested object, and the page needs to know
    // which so it can search it the right way
    let type_of = |name: &str| -> Option<String> {
        read.iter()
            .find(|f| f.get("name").and_then(|v| v.as_str()) == Some(name))
            .and_then(|f| f.get("type"))
            .and_then(|v| v.as_str())
            .map(String::from)
    };
    let mut sub_types: Vec<(usize, Value)> = Vec::new();
    for (i, field) in read.iter().enumerate() {
        let name = field.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if !name.contains('.') {
            continue;
        }
        let parts: Vec<&str> = name.split('.').collect();
        // the parents, nearest first
        let parents: Vec<String> = (1..parts.len()).rev().map(|n| parts[..n].join(".")).collect();
        let mut sub = Map::new();
        if let Some(first) = parents.first() {
            match type_of(first) {
                Some(t) if t != "object" && t != "nested" => {
                    sub.insert("multi".into(), json!({"parent": first}));
                }
                _ => {}
            }
        }
        if let Some(nested) = parents.iter().find(|p| type_of(p).as_deref() == Some("nested")) {
            sub.insert("nested".into(), json!({"path": nested}));
        }
        if !sub.is_empty() {
            sub_types.push((i, Value::Object(sub)));
        }
    }
    for (i, sub) in sub_types {
        read[i].insert("subType".into(), sub);
    }
    let mut out: Vec<Map<String, Value>> = read
        .into_iter()
        .filter(|f| !matches!(f.get("type").and_then(|v| v.as_str()), Some("object" | "nested")))
        .filter(|f| !f.get("name").and_then(|v| v.as_str()).unwrap_or("").starts_with('_'))
        .collect();
    for meta in meta_fields {
        if out.iter().any(|f| f.get("name").and_then(|v| v.as_str()) == Some(meta)) {
            continue;
        }
        let mut field = Map::new();
        if let Some(known) = by_name.get(meta).and_then(|v| v.as_object()) {
            // a meta field the engine does know is described as it is
            let engine_type = known.keys().next().map(|s| s.as_str()).unwrap_or("");
            let cap = known.values().next().cloned().unwrap_or_default();
            let aggregatable = cap.get("aggregatable").and_then(|v| v.as_bool()).unwrap_or(false);
            field.insert("name".into(), json!(meta));
            field.insert("type".into(), json!(page_type(engine_type)));
            field.insert("esTypes".into(), json!([engine_type]));
            field.insert(
                "searchable".into(),
                json!(cap.get("searchable").and_then(|v| v.as_bool()).unwrap_or(false)),
            );
            field.insert("aggregatable".into(), json!(aggregatable));
            field.insert(
                "readFromDocValues".into(),
                json!(read_from_doc_values(aggregatable, engine_type)),
            );
        } else {
            field.insert("name".into(), json!(meta));
            field.insert("type".into(), json!("string"));
            field.insert("searchable".into(), json!(false));
            field.insert("aggregatable".into(), json!(false));
            field.insert("readFromDocValues".into(), json!(false));
        }
        out.push(field);
    }
    // the meta fields the server being replaced knows something about are
    // given that shape whatever the engine said
    for field in &mut out {
        let name = field.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        match name.as_str() {
            "_source" => {
                field.insert("type".into(), json!("_source"));
            }
            "_index" | "_type" | "_id" => {
                field.insert("type".into(), json!("string"));
            }
            "_timestamp" => {
                field.insert("type".into(), json!("date"));
                field.insert("searchable".into(), json!(true));
                field.insert("aggregatable".into(), json!(true));
            }
            "_score" => {
                field.insert("type".into(), json!("number"));
                field.insert("searchable".into(), json!(false));
                field.insert("aggregatable".into(), json!(false));
            }
            _ => {}
        }
    }
    out.sort_by(|a, b| {
        let name = |f: &Map<String, Value>| {
            f.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string()
        };
        name(a).cmp(&name(b))
    });
    out.into_iter().map(Value::Object).collect()
}

/// The indices a time pattern matches, newest first.
fn resolve_time_pattern(engine: &Engine, pattern: &str) -> Result<Vec<String>, Failed> {
    let wildcard = time_pattern_to_wildcard(pattern);
    let path = format!(
        "/{}/_alias?ignore_unavailable=true&allow_no_indices=false",
        percent_encoding::utf8_percent_encode(&wildcard, percent_encoding::NON_ALPHANUMERIC)
            .to_string()
            .replace("%2A", "*")
    );
    let found = engine.call("GET", &path, None).map_err(|e| Failed::of(404, e.message))?;
    if found.get("error").is_some() {
        return Err(no_matching_indices(pattern));
    }
    let mut names: Vec<String> = Vec::new();
    if let Some(indices) = found.as_object() {
        for (name, index) in indices {
            names.push(name.clone());
            if let Some(aliases) = index.get("aliases").and_then(|v| v.as_object()) {
                names.extend(aliases.keys().cloned());
            }
        }
    }
    names.sort();
    names.dedup();
    let format = TimeFormat::parse(pattern);
    let mut dated: Vec<(String, Vec<i64>)> = names
        .into_iter()
        .filter_map(|name| {
            let parsed = format.read(&name)?;
            (format.write(&parsed) == name).then(|| (name, parsed.order()))
        })
        .collect();
    dated.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(dated.into_iter().map(|(name, _)| name).collect())
}

/// `[logs-]YYYY.MM.DD` as `logs-*`: the literal parts kept and each run of
/// date tokens a star.
pub fn time_pattern_to_wildcard(pattern: &str) -> String {
    let mut out = String::new();
    let mut in_escape = false;
    let mut in_pattern = false;
    for ch in pattern.chars() {
        match ch {
            '[' => {
                in_pattern = false;
                if !in_escape {
                    in_escape = true;
                } else {
                    out.push(ch);
                }
            }
            ']' => {
                if in_escape {
                    in_escape = false;
                } else if !in_pattern {
                    out.push(ch);
                }
            }
            _ => {
                if in_escape {
                    out.push(ch);
                } else if !in_pattern {
                    out.push('*');
                    in_pattern = true;
                }
            }
        }
    }
    out
}

/// A moment-style date format, as far as index names use one.
#[derive(Debug, PartialEq)]
enum Piece {
    Literal(String),
    Year4,
    Year2,
    Month2,
    Month1,
    Day2,
    Day1,
    Hour2,
    Hour1,
    Minute2,
    Second2,
    Week2,
    Week1,
    WeekYear4,
}

struct TimeFormat {
    pieces: Vec<Piece>,
}

#[derive(Default)]
struct Parsed {
    year: Option<i64>,
    month: Option<i64>,
    day: Option<i64>,
    hour: Option<i64>,
    minute: Option<i64>,
    second: Option<i64>,
    week: Option<i64>,
}

impl Parsed {
    fn order(&self) -> Vec<i64> {
        vec![
            self.year.unwrap_or(1970),
            self.month.unwrap_or(1),
            self.week.unwrap_or(0),
            self.day.unwrap_or(1),
            self.hour.unwrap_or(0),
            self.minute.unwrap_or(0),
            self.second.unwrap_or(0),
        ]
    }
}

impl TimeFormat {
    fn parse(pattern: &str) -> TimeFormat {
        let chars: Vec<char> = pattern.chars().collect();
        let mut pieces = Vec::new();
        let mut i = 0;
        let mut literal = String::new();
        let take = |pieces: &mut Vec<Piece>, literal: &mut String| {
            if !literal.is_empty() {
                pieces.push(Piece::Literal(std::mem::take(literal)));
            }
        };
        while i < chars.len() {
            if chars[i] == '[' {
                let mut j = i + 1;
                while j < chars.len() && chars[j] != ']' {
                    literal.push(chars[j]);
                    j += 1;
                }
                i = j + 1;
                continue;
            }
            let rest: String = chars[i..].iter().collect();
            let tokens: [(&str, Piece); 13] = [
                ("YYYY", Piece::Year4),
                ("GGGG", Piece::WeekYear4),
                ("YY", Piece::Year2),
                ("MM", Piece::Month2),
                ("DD", Piece::Day2),
                ("HH", Piece::Hour2),
                ("mm", Piece::Minute2),
                ("ss", Piece::Second2),
                ("ww", Piece::Week2),
                ("M", Piece::Month1),
                ("D", Piece::Day1),
                ("H", Piece::Hour1),
                ("w", Piece::Week1),
            ];
            let mut matched = false;
            for (token, piece) in tokens {
                if rest.starts_with(token) {
                    take(&mut pieces, &mut literal);
                    pieces.push(piece);
                    i += token.len();
                    matched = true;
                    break;
                }
            }
            if !matched {
                literal.push(chars[i]);
                i += 1;
            }
        }
        take(&mut pieces, &mut literal);
        TimeFormat { pieces }
    }

    /// The name read strictly by the format, or nothing where it does not
    /// fit.
    fn read(&self, name: &str) -> Option<Parsed> {
        let mut at = 0;
        let mut out = Parsed::default();
        let digits = |name: &str, at: usize, min: usize, max: usize| -> Option<(i64, usize)> {
            let run: String =
                name[at..].chars().take(max).take_while(|c| c.is_ascii_digit()).collect();
            if run.len() < min {
                return None;
            }
            Some((run.parse().ok()?, run.len()))
        };
        for piece in &self.pieces {
            match piece {
                Piece::Literal(text) => {
                    if !name[at..].starts_with(text.as_str()) {
                        return None;
                    }
                    at += text.len();
                }
                Piece::Year4 | Piece::WeekYear4 => {
                    let (v, n) = digits(name, at, 4, 4)?;
                    out.year = Some(v);
                    at += n;
                }
                Piece::Year2 => {
                    let (v, n) = digits(name, at, 2, 2)?;
                    out.year = Some(if v > 68 { 1900 + v } else { 2000 + v });
                    at += n;
                }
                Piece::Month2 | Piece::Month1 => {
                    let (min, max) = if *piece == Piece::Month2 { (2, 2) } else { (1, 2) };
                    let (v, n) = digits(name, at, min, max)?;
                    if !(1..=12).contains(&v) {
                        return None;
                    }
                    out.month = Some(v);
                    at += n;
                }
                Piece::Day2 | Piece::Day1 => {
                    let (min, max) = if *piece == Piece::Day2 { (2, 2) } else { (1, 2) };
                    let (v, n) = digits(name, at, min, max)?;
                    if !(1..=31).contains(&v) {
                        return None;
                    }
                    out.day = Some(v);
                    at += n;
                }
                Piece::Hour2 | Piece::Hour1 => {
                    let (min, max) = if *piece == Piece::Hour2 { (2, 2) } else { (1, 2) };
                    let (v, n) = digits(name, at, min, max)?;
                    if v > 23 {
                        return None;
                    }
                    out.hour = Some(v);
                    at += n;
                }
                Piece::Minute2 => {
                    let (v, n) = digits(name, at, 2, 2)?;
                    if v > 59 {
                        return None;
                    }
                    out.minute = Some(v);
                    at += n;
                }
                Piece::Second2 => {
                    let (v, n) = digits(name, at, 2, 2)?;
                    if v > 59 {
                        return None;
                    }
                    out.second = Some(v);
                    at += n;
                }
                Piece::Week2 | Piece::Week1 => {
                    let (min, max) = if *piece == Piece::Week2 { (2, 2) } else { (1, 2) };
                    let (v, n) = digits(name, at, min, max)?;
                    if !(1..=53).contains(&v) {
                        return None;
                    }
                    out.week = Some(v);
                    at += n;
                }
            }
        }
        (at == name.len()).then_some(out)
    }

    /// What was read, written back by the format: equal to the name when
    /// the name was written by this format and nothing else.
    fn write(&self, parsed: &Parsed) -> String {
        let mut out = String::new();
        for piece in &self.pieces {
            match piece {
                Piece::Literal(text) => out.push_str(text),
                Piece::Year4 | Piece::WeekYear4 => {
                    out.push_str(&format!("{:04}", parsed.year.unwrap_or(1970)))
                }
                Piece::Year2 => {
                    out.push_str(&format!("{:02}", parsed.year.unwrap_or(1970).rem_euclid(100)))
                }
                Piece::Month2 => out.push_str(&format!("{:02}", parsed.month.unwrap_or(1))),
                Piece::Month1 => out.push_str(&format!("{}", parsed.month.unwrap_or(1))),
                Piece::Day2 => out.push_str(&format!("{:02}", parsed.day.unwrap_or(1))),
                Piece::Day1 => out.push_str(&format!("{}", parsed.day.unwrap_or(1))),
                Piece::Hour2 => out.push_str(&format!("{:02}", parsed.hour.unwrap_or(0))),
                Piece::Hour1 => out.push_str(&format!("{}", parsed.hour.unwrap_or(0))),
                Piece::Minute2 => out.push_str(&format!("{:02}", parsed.minute.unwrap_or(0))),
                Piece::Second2 => out.push_str(&format!("{:02}", parsed.second.unwrap_or(0))),
                Piece::Week2 => out.push_str(&format!("{:02}", parsed.week.unwrap_or(1))),
                Piece::Week1 => out.push_str(&format!("{}", parsed.week.unwrap_or(1))),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_pattern_becomes_a_wildcard() {
        assert_eq!(time_pattern_to_wildcard("[logs-]YYYY.MM.DD"), "logs-*");
        assert_eq!(time_pattern_to_wildcard("[logs-]YYYY.MM.DD[-suffix]"), "logs-*-suffix");
        assert_eq!(time_pattern_to_wildcard("YYYY[.]MM"), "*.*");
        assert_eq!(time_pattern_to_wildcard("[[escaped]]"), "[escaped]");
    }

    #[test]
    fn a_name_is_read_strictly_and_written_back() {
        let format = TimeFormat::parse("[logs-]YYYY.MM.DD");
        let parsed = format.read("logs-2017.01.02").expect("a match");
        assert_eq!(format.write(&parsed), "logs-2017.01.02");
        assert!(format.read("logs-2017.1.2").is_none());
        assert!(format.read("logs-2017.13.02").is_none());
        assert!(format.read("logs-2017.01.02-extra").is_none());
        assert!(format.read("other-2017.01.02").is_none());
    }

    #[test]
    fn the_engine_answer_folds_to_one_entry_per_field() {
        let caps = json!({"fields": {
            "@timestamp": {"date": {"type": "date", "searchable": true, "aggregatable": true}},
            "success": {
                "boolean": {"type": "boolean", "searchable": true, "aggregatable": true,
                            "indices": ["logs-2017.01.02"]},
                "keyword": {"type": "keyword", "searchable": true, "aggregatable": true,
                            "indices": ["logs-2017.01.01"]},
            },
            "number_conflict": {
                "integer": {"type": "integer", "searchable": true, "aggregatable": true},
                "float": {"type": "float", "searchable": true, "aggregatable": true},
            },
            "baz": {"text": {"type": "text", "searchable": true, "aggregatable": false}},
            "baz.keyword": {"keyword": {"type": "keyword", "searchable": true, "aggregatable": true}},
            "nestedField": {"nested": {"type": "nested", "searchable": false, "aggregatable": false}},
            "nestedField.child": {"keyword": {"type": "keyword", "searchable": true, "aggregatable": true}},
            "_id": {"_id": {"type": "_id", "searchable": true, "aggregatable": true}},
        }});
        let out = fold(&caps, &["_id".to_string(), "meta".to_string()]);
        let names: Vec<&str> = out.iter().map(|f| f["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "@timestamp",
                "_id",
                "baz",
                "baz.keyword",
                "meta",
                "nestedField.child",
                "number_conflict",
                "success"
            ]
        );
        let by = |name: &str| out.iter().find(|f| f["name"] == name).unwrap().clone();
        assert_eq!(by("success")["type"], "conflict");
        assert_eq!(by("success")["conflictDescriptions"]["keyword"], json!(["logs-2017.01.01"]));
        assert_eq!(by("number_conflict")["type"], "number");
        assert_eq!(by("number_conflict")["readFromDocValues"], true);
        assert_eq!(by("baz.keyword")["subType"], json!({"multi": {"parent": "baz"}}));
        assert_eq!(by("nestedField.child")["subType"], json!({"nested": {"path": "nestedField"}}));
        assert_eq!(
            by("meta"),
            json!({"name": "meta", "type": "string", "searchable": false,
                                       "aggregatable": false, "readFromDocValues": false})
        );
        assert_eq!(by("_id")["readFromDocValues"], false);
        assert_eq!(by("_id")["type"], "string");
    }
}
