//! Reading terms from somewhere other than the request: another document, a
//! bitmap, or a document the caller says looks like what they want.

use super::*;

/// Replace `terms: {field: {index, id, path}}` with the terms held by that
/// document, the way OpenSearch resolves a terms lookup before searching.
/// Read a RoaringBitmap in its portable serialisation back into the integers
/// it holds.
///
/// A bitmap is how a caller sends a very long terms list compactly: the ids
/// are grouped by their high sixteen bits, and each group is written either as
/// a sorted array of the low bits or as a bitset over them.
/// The 64-bit form: a count of high words, then each high word followed by an
/// ordinary 32-bit bitmap of the low half.
pub(crate) fn decode_roaring64(bytes: &[u8]) -> Option<Vec<i64>> {
    let u32_at = |i: usize| -> Option<u32> {
        Some(u32::from_le_bytes([
            *bytes.get(i)?,
            *bytes.get(i + 1)?,
            *bytes.get(i + 2)?,
            *bytes.get(i + 3)?,
        ]))
    };
    let count = u32_at(0)? as usize;
    // the count is written as eight bytes, whose upper half is always zero
    if u32_at(4)? != 0 {
        return None;
    }
    let mut at = 8;
    let mut out = Vec::new();
    for _ in 0..count {
        let high = u32_at(at)? as i64;
        at += 4;
        let (low, used) = decode_roaring_at(bytes, at)?;
        at += used;
        out.extend(low.into_iter().map(|v| high << 32 | v));
    }
    Some(out)
}

pub(crate) fn decode_roaring(bytes: &[u8]) -> Option<Vec<i64>> {
    decode_roaring_at(bytes, 0).map(|(v, _)| v)
}

pub(crate) fn decode_roaring_at(bytes: &[u8], start: usize) -> Option<(Vec<i64>, usize)> {
    let bytes = bytes.get(start..)?;
    decode_roaring_inner(bytes)
}

pub(crate) fn decode_roaring_inner(bytes: &[u8]) -> Option<(Vec<i64>, usize)> {
    let u16_at = |i: usize| -> Option<u16> {
        Some(u16::from_le_bytes([*bytes.get(i)?, *bytes.get(i + 1)?]))
    };
    let u32_at = |i: usize| -> Option<u32> {
        Some(u32::from_le_bytes([
            *bytes.get(i)?,
            *bytes.get(i + 1)?,
            *bytes.get(i + 2)?,
            *bytes.get(i + 3)?,
        ]))
    };
    let cookie = u32_at(0)?;
    let mut at = 4;
    // the older cookie carries the container count separately; the newer one
    // packs it into the cookie and is followed by a bitset saying which
    // containers are run-encoded
    let (count, has_runs) = if cookie & 0xffff == 12_347 {
        (((cookie >> 16) + 1) as usize, true)
    } else if cookie == 12_346 {
        let n = u32_at(at)? as usize;
        at += 4;
        (n, false)
    } else {
        return None;
    };
    let mut runs = vec![false; count];
    if has_runs {
        let bytes_needed = count.div_ceil(8);
        for (i, run) in runs.iter_mut().enumerate() {
            *run = bytes.get(at + i / 8).map(|b| b >> (i % 8) & 1 == 1).unwrap_or(false);
        }
        at += bytes_needed;
    }
    let mut keys = Vec::with_capacity(count);
    for i in 0..count {
        keys.push((u16_at(at + i * 4)?, u16_at(at + i * 4 + 2)? as u32 + 1));
    }
    at += count * 4;
    // the offset header is only written when there are no runs, and the
    // containers follow it either way
    if !has_runs || count >= 4 {
        at += count * 4;
    }
    let mut out = Vec::new();
    for (i, (key, card)) in keys.iter().enumerate() {
        let high = (*key as i64) << 16;
        if runs[i] {
            let n = u16_at(at)? as usize;
            at += 2;
            for _ in 0..n {
                let start = u16_at(at)? as i64;
                let len = u16_at(at + 2)? as i64;
                at += 4;
                for v in start..=start + len {
                    out.push(high | v);
                }
            }
        } else if *card <= 4096 {
            for _ in 0..*card {
                out.push(high | u16_at(at)? as i64);
                at += 2;
            }
        } else {
            for word in 0..1024 {
                let mut bits = 0u64;
                for b in 0..8 {
                    bits |= (*bytes.get(at + word * 8 + b)? as u64) << (b * 8);
                }
                for bit in 0..64 {
                    if bits >> bit & 1 == 1 {
                        out.push(high | (word as i64 * 64 + bit));
                    }
                }
            }
            at += 8192;
        }
    }
    Some((out, at))
}

/// A `terms` clause may carry its list as a bitmap rather than as an array.
pub(crate) fn expand_bitmap_terms(node: &mut Value) {
    let Some(o) = node.as_object_mut() else { return };
    let is_bitmap =
        o.get("terms").and_then(|t| t.get("value_type")).and_then(|v| v.as_str()) == Some("bitmap");
    if is_bitmap {
        if let Some(terms) = o.get_mut("terms").and_then(|t| t.as_object_mut()) {
            terms.remove("value_type");
            let fields: Vec<String> = terms.keys().cloned().collect();
            for f in fields {
                let encoded = match terms.get(&f) {
                    Some(Value::String(b)) => Some(b.clone()),
                    Some(Value::Array(a)) if a.len() == 1 => a[0].as_str().map(|s| s.to_string()),
                    _ => None,
                };
                let Some(encoded) = encoded else { continue };
                // the 32-bit form starts with its cookie; the 64-bit form
                // starts with a count of the high words it groups by
                let Some(values) = base64_decode(&encoded)
                    .as_deref()
                    .and_then(|b| decode_roaring(b).or_else(|| decode_roaring64(b)))
                else {
                    continue;
                };
                terms.insert(f, Value::Array(values.into_iter().map(|v| json!(v)).collect()));
            }
        }
    }
    for (_, v) in o.iter_mut() {
        match v {
            Value::Object(_) => expand_bitmap_terms(v),
            Value::Array(a) => a.iter_mut().for_each(expand_bitmap_terms),
            _ => {}
        }
    }
}

pub(crate) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut acc: u32 = 0;
    let mut bits = 0;
    let mut out = Vec::new();
    for c in text.bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = TABLE.iter().position(|t| *t == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Rewrite a `more_like_this` clause into the query it stands for.
///
/// The clause names documents rather than terms: the terms come from
/// analysing what those documents hold. `unlike` names documents whose terms
/// are to be taken back out of that set, which is what makes a query for
/// "like this one but not like that one" narrower rather than empty.
pub(crate) fn expand_more_like_this(store: &Store, targets: &[String], node: &mut Value) {
    let Some(o) = node.as_object_mut() else { return };
    for (_, v) in o.iter_mut() {
        match v {
            Value::Object(_) => expand_more_like_this(store, targets, v),
            Value::Array(a) => a.iter_mut().for_each(|x| expand_more_like_this(store, targets, x)),
            _ => {}
        }
    }
    let Some(spec) = o.get("more_like_this").cloned() else { return };

    let listed = |key: &str| -> Vec<Value> {
        match spec.get(key) {
            Some(Value::Array(a)) => a.clone(),
            Some(one) => vec![one.clone()],
            None => Vec::new(),
        }
    };
    let fields: Option<Vec<String>> = spec
        .get("fields")
        .and_then(|f| f.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect());

    // the documents an item names, or the one it carries
    let source_of_item = |item: &Value| -> Option<(Option<String>, Value)> {
        if let Some(doc) = item.get("doc") {
            return Some((None, doc.clone()));
        }
        let id = match item {
            Value::String(s) => s.clone(),
            other => other.get("_id").map(|v| match v {
                Value::String(s) => s.clone(),
                n => n.to_string(),
            })?,
        };
        let index = item.get("_index").and_then(|v| v.as_str()).map(|s| s.to_string());
        let names: Vec<String> = match index {
            Some(n) => vec![n],
            None => targets.to_vec(),
        };
        for n in names {
            let st = store.get(&n)?;
            let g = st.read();
            if let Some(src) = crate::api::read_source(&g, &id) {
                return Some((Some(id), src));
            }
        }
        None
    };

    // words a document contributes, by field
    let collect = |items: &[Value],
                   out: &mut std::collections::BTreeMap<String, Vec<String>>,
                   ids: &mut Vec<String>| {
        for item in items {
            let Some((id, src)) = source_of_item(item) else { continue };
            if let Some(id) = id {
                ids.push(id);
            }
            let Some(obj) = src.as_object() else { continue };
            for (name, value) in obj {
                if fields.as_ref().map(|f| !f.iter().any(|w| w == name)).unwrap_or(false) {
                    continue;
                }
                let Some(text) = value.as_str() else { continue };
                for word in text.split_whitespace() {
                    out.entry(name.clone()).or_default().push(word.to_lowercase());
                }
            }
        }
    };

    let mut like: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    let mut unlike: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    let mut like_ids = Vec::new();
    let mut unlike_ids = Vec::new();
    collect(&listed("like"), &mut like, &mut like_ids);
    collect(&listed("unlike"), &mut unlike, &mut unlike_ids);

    let min_tf = spec.get("min_term_freq").and_then(|v| v.as_u64()).unwrap_or(2);
    let min_df = spec.get("min_doc_freq").and_then(|v| v.as_u64()).unwrap_or(5);

    let mut should = Vec::new();
    for (field, words) in &like {
        let taken_back: Vec<&String> =
            unlike.get(field).map(|w| w.iter().collect()).unwrap_or_default();
        let mut counts: std::collections::BTreeMap<&String, u64> = Default::default();
        for w in words {
            *counts.entry(w).or_insert(0) += 1;
        }
        for (word, tf) in counts {
            if tf < min_tf || taken_back.contains(&word) {
                continue;
            }
            // how many documents hold the word, which is what min_doc_freq caps
            let df = targets
                .iter()
                .filter_map(|n| store.get(n))
                .map(|st| {
                    let g = st.read();
                    let ctx = Ctx {
                        fields: &g.fields,
                        mapping: &g.mapping,
                        index: &g.index,
                        max_terms_count: g.max_terms_count(),
                        max_regex_length: g.max_regex_length(),
                        allow_expensive: crate::search::expensive_allowed(store),
                        observed_kinds: &g.observed_kinds,
                        kinds_complete: g.kinds_complete,
                        stats: &g.stats,
                    };
                    crate::query::build(&ctx, &json!({"match": {field.clone(): word}}))
                        .ok()
                        .and_then(|q| g.reader.searcher().search(&q, &Count).ok())
                        .unwrap_or(0) as u64
                })
                .sum::<u64>();
            if df < min_df {
                continue;
            }
            should.push(json!({"match": {field.clone(): word}}));
        }
    }

    let mut bool_q = serde_json::Map::new();
    if should.is_empty() {
        // nothing survived the thresholds, so nothing is like it
        *node = json!({"bool": {"must_not": [{"match_all": {}}]}});
        return;
    }
    bool_q.insert("should".into(), Value::Array(should));
    bool_q.insert("minimum_should_match".into(), json!(1));
    // the documents the query was built from are left out unless asked for
    let include = spec.get("include").and_then(|v| v.as_bool()).unwrap_or(false);
    if !include && !like_ids.is_empty() {
        bool_q.insert("must_not".into(), json!([{"terms": {"_id": like_ids}}]));
    }
    o.remove("more_like_this");
    *node = json!({"bool": Value::Object(bool_q)});
}

pub(crate) fn resolve_terms_lookups(
    store: &Store,
    node: &mut Value,
) -> std::result::Result<(), Response> {
    match node {
        Value::Object(o) => {
            if let Some(Value::Object(spec)) = o.get("terms").cloned() {
                for (field, def) in spec {
                    let Some(d) = def.as_object() else { continue };
                    let (Some(index), Some(path)) = (
                        d.get("index").and_then(|v| v.as_str()),
                        d.get("path").and_then(|v| v.as_str()),
                    ) else {
                        continue;
                    };
                    let Some(st) = store.get(index) else {
                        return Err(no_such_index(index));
                    };
                    let pointer = format!("/{}", path.replace('.', "/"));
                    // the terms come from one named document, or from every
                    // document a query finds -- the second is how a caller
                    // says "whatever this group follows"
                    let list: Vec<Value> = if let Some(id) = d.get("id").and_then(|v| v.as_str()) {
                        let g = st.read();
                        let values = crate::api::read_source(&g, id)
                            .and_then(|src| src.pointer(&pointer).cloned())
                            .unwrap_or(Value::Array(vec![]));
                        match values {
                            Value::Array(a) => a,
                            other => vec![other],
                        }
                    } else if let Some(q) = d.get("query") {
                        let g = st.read();
                        let ctx = Ctx {
                            fields: &g.fields,
                            mapping: &g.mapping,
                            index: &g.index,
                            max_terms_count: g.max_terms_count(),
                            max_regex_length: g.max_regex_length(),
                            allow_expensive: crate::search::expensive_allowed(store),
                            observed_kinds: &g.observed_kinds,
                            kinds_complete: g.kinds_complete,
                            stats: &g.stats,
                        };
                        let built = crate::query::build(&ctx, q).map_err(|e| {
                            err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string())
                        })?;
                        let searcher = g.reader.searcher();
                        let hits = searcher
                            .search(
                                &built,
                                &TopDocs::with_limit(g.max_terms_count()).order_by_score(),
                            )
                            .map_err(|e| {
                                err(
                                    StatusCode::BAD_REQUEST,
                                    "search_phase_execution_exception",
                                    e.to_string(),
                                )
                            })?;
                        let mut out: Vec<Value> = Vec::new();
                        for (_, addr) in hits {
                            let Some((_, src)) = source_of(&searcher, &g, addr) else { continue };
                            // a document with nothing at that path contributes
                            // nothing, which is not the same as contributing a null
                            match src.pointer(&pointer) {
                                Some(Value::Array(a)) => {
                                    out.extend(a.iter().filter(|v| !v.is_null()).cloned())
                                }
                                Some(Value::Null) | None => {}
                                Some(one) => out.push(one.clone()),
                            }
                        }
                        out.sort_by_key(|v| v.to_string());
                        out.dedup();
                        out
                    } else {
                        continue;
                    };
                    // a lookup may point at a bitmap, whose value_type sits
                    // beside the field rather than inside it
                    let vt = o.get("terms").and_then(|t| t.get("value_type")).cloned();
                    let mut terms = json!({ field: list });
                    if let Some(vt) = vt {
                        terms["value_type"] = vt;
                    }
                    o.insert("terms".into(), terms);
                }
            }
            for (_, v) in o.iter_mut() {
                resolve_terms_lookups(store, v)?;
            }
            Ok(())
        }
        Value::Array(a) => {
            for v in a {
                resolve_terms_lookups(store, v)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
