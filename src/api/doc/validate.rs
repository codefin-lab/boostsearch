//! Whether a write may go ahead: the document, the version, the routing.

use super::*;

/// Write one document. `op_type == "create"` refuses to overwrite.
pub fn append_only(st: &IdxState) -> bool {
    st.setting("append_only.enabled").map(|v| v == "true").unwrap_or(false)
}

/// A version the caller supplied, and what it means for the write.
///
/// `external` numbers a document from somewhere outside the index: it must
/// climb, so a write carrying a version the index already has, or an older
/// one, has arrived out of order and is refused. `external_gte` allows the
/// same number again. Without a type the number names the version the caller
/// believes is current, and must match.
pub(crate) fn version_check(
    st: &IdxState,
    id: &str,
    p: &Params,
) -> std::result::Result<Option<u64>, Response> {
    let Some(want) = p.get("version").and_then(|v| v.parse::<u64>().ok()) else {
        return Ok(None);
    };
    let ty = p.get("version_type").map(|v| v.as_str()).unwrap_or("internal");
    let existed = exists_doc(st, id);
    let have = st.version_of(id);
    let conflict = |have: u64| {
        Err(err(
            StatusCode::CONFLICT,
            "version_conflict_engine_exception",
            format!(
                "[{id}]: version conflict, current version [{have}] is higher or equal to \
                 the one provided [{want}]"
            ),
        ))
    };
    match ty {
        "external" | "external_gt" => {
            if existed && want <= have {
                return conflict(have);
            }
            Ok(Some(want))
        }
        "external_gte" => {
            if existed && want < have {
                return conflict(have);
            }
            Ok(Some(want))
        }
        _ => {
            if !existed || want != have {
                return Err(err(
                    StatusCode::CONFLICT,
                    "version_conflict_engine_exception",
                    format!(
                        "[{id}]: version conflict, required version [{want}] is different \
                         to the one in the index [{have}]"
                    ),
                ));
            }
            Ok(None)
        }
    }
}

/// `if_seq_no` makes a write conditional on the document not having moved.
pub(crate) fn seq_check(st: &IdxState, id: &str, p: &Params) -> Option<Response> {
    let want_seq = p.get("if_seq_no").and_then(|v| v.parse::<u64>().ok());
    let want_term = p.get("if_primary_term").and_then(|v| v.parse::<u64>().ok());
    if want_seq.is_none() && want_term.is_none() {
        return None;
    }
    if !exists_doc(st, id) {
        // a condition on a document that is not there cannot hold
        let want = want_seq.unwrap_or(0);
        let want_term = want_term.unwrap_or(1);
        return Some(err(
            StatusCode::CONFLICT,
            "version_conflict_engine_exception",
            format!(
                "[{id}]: version conflict, required seqNo [{want}], primary term [{want_term}]. \
                 but no document was found"
            ),
        ));
    }
    let have = read_seq(st, id).unwrap_or(0);
    // a shard that has never failed over is on its first term, so any other
    // term the caller insists on is a term this document was not written in
    let term_ok = want_term.map(|t| t == 1).unwrap_or(true);
    let want = want_seq.unwrap_or(have);
    if have == want && term_ok {
        return None;
    }
    let want_term = want_term.unwrap_or(1);
    Some(err(
        StatusCode::CONFLICT,
        "version_conflict_engine_exception",
        format!(
            "[{id}]: version conflict, required seqNo [{want}], primary term \
             [{want_term}]. current document has seqNo [{have}] and primary term [1]"
        ),
    ))
}

/// `raw` is the document exactly as the client sent it; passing it through
/// avoids re-serialising a tree we only just parsed.
/// What is wrong with this document, as a kind, a reason and its cause.
///
/// A write and one item of a bulk request both need to say the same thing, so
/// neither of them decides it.
pub fn document_complaint(st: &IdxState, source: &Value) -> Option<(String, String, String)> {
    // a date_nanos counts nanoseconds in an i64, which begins in 1970 and runs
    // out in 2262
    for name in st.mapping.nanos_fields().iter() {
        let Some(value) = source.pointer(&format!("/{}", name.replace('.', "/"))) else {
            continue;
        };
        let texts: Vec<String> = match value {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
            _ => Vec::new(),
        };
        for text in texts {
            // read the text as written: the usual path folds a date through
            // the resolution the index keeps, which is the very thing being
            // checked for
            let Some(dt) = boostcore::time::OffsetDateTime::parse(
                &text,
                &boostcore::time::format_description::well_known::Rfc3339,
            )
            .ok()
            .or_else(|| crate::store::parse_date_lenient(&text)) else {
                continue;
            };
            let nanos = dt.unix_timestamp_nanos();
            let complaint = if dt.year() < 1970 {
                Some(format!(
                    "date[{text}] is before the epoch in 1970 and cannot be stored in \
                     nanosecond resolution"
                ))
            } else if nanos > i64::MAX as i128 {
                Some(format!(
                    "date[{text}] is after 2262-04-11T23:47:16.854775807 and cannot be stored \
                     in nanosecond resolution"
                ))
            } else {
                None
            };
            if let Some(reason) = complaint {
                return Some((
                    "mapper_parsing_exception".into(),
                    format!("failed to parse field [{name}] of type [date_nanos]"),
                    reason,
                ));
            }
        }
    }
    // a nested field is a list of documents of its own, and an index says how
    // many of them one document may carry
    let nested_limit = st.numeric_setting("mapping.nested_objects.limit").unwrap_or(10_000);
    let mut nested_count = 0u64;
    for (name, kind) in st.mapping.types.iter() {
        if kind != "nested" {
            continue;
        }
        if let Some(Value::Array(a)) = source.pointer(&format!("/{}", name.replace('.', "/"))) {
            nested_count += a.len() as u64;
        }
    }
    if nested_count > nested_limit {
        return Some((
            "illegal_argument_exception".into(),
            format!(
                "The number of nested documents has exceeded the allowed limit of \
                 [{nested_limit}]. This limit can be set by changing the \
                 [index.mapping.nested_objects.limit] index level setting."
            ),
            String::new(),
        ));
    }
    // a flat_object keeps whatever object it is given; it is not a place to
    // put a string
    for (name, kind) in st.mapping.types.iter() {
        if kind != "flat_object" {
            continue;
        }
        let Some(value) = source.pointer(&format!("/{}", name.replace('.', "/"))) else {
            continue;
        };
        let ok = match value {
            Value::Object(_) | Value::Null => true,
            Value::Array(a) => a.iter().all(|v| v.is_object() || v.is_null()),
            _ => false,
        };
        if !ok {
            return Some((
                "parsing_exception".into(),
                format!("Failed to parse field [{name}] of type [flat_object]"),
                String::new(),
            ));
        }
    }
    // a completion field filed under contexts has to be given them: without
    // one the value could never be found again
    if let Some(props) = st.mapping.raw.pointer("/properties").and_then(|p| p.as_object()) {
        for (name, def) in props {
            let needs = def
                .get("contexts")
                .and_then(|c| c.as_array())
                .map(|c| c.iter().all(|d| d.get("path").is_none()) && !c.is_empty())
                .unwrap_or(false);
            if !needs {
                continue;
            }
            let Some(value) = source.get(name) else { continue };
            let given = match value {
                Value::Object(o) => o.contains_key("contexts"),
                Value::Array(a) => a
                    .iter()
                    .all(|v| v.as_object().map(|o| o.contains_key("contexts")).unwrap_or(false)),
                _ => false,
            };
            if !given {
                return Some((
                    "mapper_parsing_exception".into(),
                    format!("Contexts are mandatory in context enabled completion field [{name}]"),
                    String::new(),
                ));
            }
        }
    }
    None
}

/// `require_alias` says the write is only meant for an alias, so a name that
/// is not one is treated as absent rather than created on the spot.
pub(crate) fn refuse_unless_alias(store: &Store, index: &str, p: &Params) -> Option<Response> {
    let asked = p.get("require_alias").map(|v| v != "false").unwrap_or(false);
    if asked && !store.is_alias(index) {
        return Some(err(
            StatusCode::NOT_FOUND,
            "index_not_found_exception",
            format!(
                "no such index [{index}] and [require_alias] request flag is [true] and [{index}] is not an alias"
            ),
        ));
    }
    None
}

/// Does the routing quoted on a request agree with the one the document was
/// written under? A document reached by the wrong routing is, to the caller,
/// not there at all: in a real cluster the request would have gone to a shard
/// that never held it.
pub(crate) fn routing_matches(st: &IdxState, id: &str, p: &Params) -> bool {
    match st.routing.get(id) {
        Some(have) => p.get("routing").map(|want| want == have).unwrap_or(false),
        None => true,
    }
}
