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
    if is_bitmap && let Some(terms) = o.get_mut("terms").and_then(|t| t.as_object_mut()) {
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
            // a string that names no document is the text itself, which is
            // how `like` is most often written
            if let Value::String(text) = item
                && source_of_item(item).is_none()
            {
                for name in fields.clone().unwrap_or_default() {
                    for word in text.split_whitespace() {
                        out.entry(name.clone()).or_default().push(word.to_lowercase());
                    }
                }
                continue;
            }
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
                        analysis: &g.analysis,
                        index: &g.index,
                        max_terms_count: g.max_terms_count(),
                        max_regex_length: g.max_regex_length(),
                        allow_expensive: crate::search::expensive_allowed(store),
                        observed_kinds: &g.observed_kinds,
                        kinds_complete: g.kinds_complete,
                        stats: &g.stats,
            vectors: &g.vectors,
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
                    let elsewhere = store.get(index).is_none();
                    if elsewhere {
                        // the index is the cluster's, not this node's: the
                        // document comes from the node holding it
                        let id = d.get("id").and_then(|v| v.as_str());
                        let from_cluster =
                            id.and_then(|id| crate::cluster::forward::fetch_document(index, id));
                        match from_cluster {
                            Some(src) => {
                                let pointer = format!("/{}", path.replace('.', "/"));
                                let list = match src.pointer(&pointer).cloned() {
                                    Some(Value::Array(a)) => a,
                                    Some(one) => vec![one],
                                    None => Vec::new(),
                                };
                                let vt = o.get("terms").and_then(|t| t.get("value_type")).cloned();
                                let mut terms = json!({ field.clone(): list });
                                if let Some(vt) = vt {
                                    terms["value_type"] = vt;
                                }
                                o.insert("terms".into(), terms);
                                continue;
                            }
                            None => return Err(no_such_index(index)),
                        }
                    }
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
                            analysis: &g.analysis,
                            index: &g.index,
                            max_terms_count: g.max_terms_count(),
                            max_regex_length: g.max_regex_length(),
                            allow_expensive: crate::search::expensive_allowed(store),
                            observed_kinds: &g.observed_kinds,
                            kinds_complete: g.kinds_complete,
                            stats: &g.stats,
            vectors: &g.vectors,
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

/// Rewrite the queries that join a document to its parent or its children.
///
/// A join field says which side of a relation a document is on and, for a
/// child, which document is its parent. Documents are stored whole here, so
/// `has_child` and `has_parent` are two passes: find the documents on one
/// side, then ask for the documents on the other side that name them.
/// Whether anywhere in the query one document is asked about through another.
pub(crate) fn names_a_join(node: &Value) -> bool {
    match node {
        Value::Object(o) => {
            o.keys().any(|k| matches!(k.as_str(), "has_child" | "has_parent" | "parent_id"))
                || o.values().any(names_a_join)
        }
        Value::Array(items) => items.iter().any(names_a_join),
        _ => false,
    }
}

pub(crate) fn expand_joins(store: &Store, targets: &[String], node: &mut Value) {
    let Some(o) = node.as_object_mut() else { return };
    for (_, v) in o.iter_mut() {
        match v {
            Value::Object(_) => expand_joins(store, targets, v),
            Value::Array(a) => a.iter_mut().for_each(|x| expand_joins(store, targets, x)),
            _ => {}
        }
    }
    let joins = ["has_child", "has_parent", "parent_id"];
    let Some(kind) = joins.iter().find(|k| o.contains_key(**k)).map(|k| k.to_string()) else {
        return;
    };
    let spec = o.get(&kind).cloned().unwrap_or(Value::Null);
    let field = join_field(store, targets);
    let Some(field) = field else { return };

    let rewritten = match kind.as_str() {
        // the documents whose children answer the inner query
        "has_child" => {
            let child = spec.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let inner = spec.get("query").cloned().unwrap_or_else(|| json!({"match_all": {}}));
            let of_that_kind = json!({
                "bool": {"must": [inner, on_that_side(&field, child)]}
            });
            let parents = ids_of_field(store, targets, &of_that_kind, &format!("{field}.parent"));
            json!({"ids": {"values": parents}})
        }
        // the documents whose parent answers the inner query
        "has_parent" => {
            let parent = spec.get("parent_type").and_then(|v| v.as_str()).unwrap_or("");
            let inner = spec.get("query").cloned().unwrap_or_else(|| json!({"match_all": {}}));
            let of_that_kind = json!({
                "bool": {"must": [inner, on_that_side(&field, parent)]}
            });
            let parents = matching_ids_here(store, targets, &of_that_kind);
            // a parent is named by its id, which a document may have written
            // as a number rather than as the string the id itself is
            let mut written: Vec<Value> = Vec::new();
            for id in &parents {
                written.push(json!(id));
                if let Ok(n) = id.parse::<i64>() {
                    written.push(json!(n));
                }
            }
            json!({"terms": {format!("{field}.parent"): written}})
        }
        // the children of one named document
        _ => {
            let parent = spec
                .get("id")
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            let child = spec.get("type").and_then(|v| v.as_str()).unwrap_or("");
            json!({
                "bool": {"must": [
                    {"term": {format!("{field}.parent"): parent}},
                    on_that_side(&field, child),
                ]}
            })
        }
    };
    o.remove(&kind);
    *node = rewritten;
}

/// Which side of the relation a document is on.
///
/// A document with no parent may write the join field as the name alone
/// rather than as an object, which is how OpenSearch lets a root document be
/// written; both spellings name the same side.
fn on_that_side(field: &str, name: &str) -> Value {
    json!({"bool": {"should": [
        {"term": {format!("{field}.name"): name}},
        {"term": {field: name}},
    ], "minimum_should_match": 1}})
}

/// The join field an index declares, if it declares one.
pub(crate) fn join_field(store: &Store, targets: &[String]) -> Option<String> {
    for name in targets {
        let st = store.get(name)?;
        let g = st.read();
        if let Some((path, _)) = g.mapping.types.iter().find(|(_, kind)| *kind == "join") {
            return Some(path.clone());
        }
    }
    None
}

/// The ids of the documents a query finds.
pub(crate) fn matching_ids_here(store: &Store, targets: &[String], query: &Value) -> Vec<String> {
    let probe = json!({"query": query, "size": 10_000, "_source": false});
    match run(store, &targets.join(","), &probe, &Params::new()) {
        Ok(found) => found
            .hits
            .iter()
            .filter_map(|hit| hit.get("_id").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// What the documents a query finds hold at one path.
pub(crate) fn ids_of_field(
    store: &Store,
    targets: &[String],
    query: &Value,
    path: &str,
) -> Vec<String> {
    let probe = json!({"query": query, "size": 10_000, "_source": [path]});
    let pointer = format!("/_source/{}", path.replace('.', "/"));
    match run(store, &targets.join(","), &probe, &Params::new()) {
        Ok(found) => found
            .hits
            .iter()
            .filter_map(|hit| {
                hit.pointer(&pointer).map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// The join clauses that asked for the documents on the other side to be
/// listed with each hit.
///
/// Collected before the joins are rewritten, since after that there is no
/// clause left to read the request from.
pub(crate) fn collect_join_inner_hits(node: &Value, out: &mut Vec<(String, String, Value, Value)>) {
    match node {
        Value::Object(o) => {
            for (kind, spec) in o {
                if matches!(kind.as_str(), "has_child" | "has_parent")
                    && let Some(inner) = spec.get("inner_hits")
                {
                    let named = match kind.as_str() {
                        "has_child" => spec.get("type"),
                        _ => spec.get("parent_type"),
                    };
                    out.push((
                        kind.clone(),
                        named.and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                        spec.get("query").cloned().unwrap_or_else(|| json!({"match_all": {}})),
                        inner.clone(),
                    ));
                }
            }
            o.values().for_each(|v| collect_join_inner_hits(v, out));
        }
        Value::Array(items) => items.iter().for_each(|v| collect_join_inner_hits(v, out)),
        _ => {}
    }
}

/// The documents on the other side of each join, listed with the hit they
/// were reached through.
pub(crate) fn attach_join_inner_hits(
    store: &Store,
    targets: &[String],
    page: &mut [Value],
    asked: &[(String, String, Value, Value)],
) {
    let Some(field) = join_field(store, targets) else { return };
    for (kind, named, inner, options) in asked {
        let label =
            options.get("name").and_then(|v| v.as_str()).unwrap_or(named.as_str()).to_string();
        let size = options.get("size").and_then(|v| v.as_u64()).unwrap_or(3) as usize;
        for hit in page.iter_mut() {
            let id = hit.get("_id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let query = match kind.as_str() {
                // the children of this document that answer the clause
                "has_child" => {
                    let mut written = vec![json!(id)];
                    if let Ok(n) = id.parse::<i64>() {
                        written.push(json!(n));
                    }
                    json!({"bool": {"must": [
                        inner.clone(),
                        on_that_side(&field, named),
                        {"terms": {format!("{field}.parent"): written}},
                    ]}})
                }
                // the document this one hangs off, where it hangs off one
                _ => {
                    let parent = hit
                        .pointer(&format!("/_source/{}/parent", field.replace('.', "/")))
                        .map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .unwrap_or_default();
                    json!({"bool": {"must": [
                        inner.clone(),
                        on_that_side(&field, named),
                        {"ids": {"values": [parent]}},
                    ]}})
                }
            };
            let mut probe = json!({"query": query, "size": size, "_source": true});
            // the listing carries whatever the request asked each of these
            // documents to carry
            for named in [
                "_source",
                "seq_no_primary_term",
                "version",
                "sort",
                "fields",
                "docvalue_fields",
                "stored_fields",
                "highlight",
                "explain",
                "from",
            ] {
                if let Some(asked) = options.get(named) {
                    probe[named] = asked.clone();
                }
            }
            let found = run(store, &targets.join(","), &probe, &Params::new());
            let (total, list) = match found {
                Ok(out) => (out.total, out.hits),
                Err(_) => (0, Vec::new()),
            };
            let section = json!({
                "hits": {
                    "total": {"value": total, "relation": "eq"},
                    "max_score": list.first().and_then(|h| h.get("_score").cloned()),
                    "hits": list,
                }
            });
            match hit.get_mut("inner_hits").and_then(|v| v.as_object_mut()) {
                Some(o) => {
                    o.insert(label.clone(), section.clone());
                }
                None => {
                    hit["inner_hits"] = json!({ label.clone(): section });
                }
            }
        }
    }
}

/// `percolate` -- the stored queries a document matches.
///
/// A percolator field holds a query. Asked which of the stored queries a
/// document would match, each query is run over that document in a scratch
/// index holding nothing else, and the clause is read as the ids of the
/// queries that found it.
pub(crate) fn expand_percolate(store: &Store, targets: &[String], node: &mut Value) {
    let Some(o) = node.as_object_mut() else { return };
    for (_, v) in o.iter_mut() {
        match v {
            Value::Object(_) => expand_percolate(store, targets, v),
            Value::Array(a) => a.iter_mut().for_each(|x| expand_percolate(store, targets, x)),
            _ => {}
        }
    }
    let Some(spec) = o.get("percolate").cloned() else { return };
    let field = spec.get("field").and_then(|v| v.as_str()).unwrap_or("query").to_string();
    // the documents to percolate: written into the request, or fetched from
    // an index by their ids
    let mut documents: Vec<Value> = Vec::new();
    if let Some(one) = spec.get("document") {
        documents.push(one.clone());
    }
    if let Some(many) = spec.get("documents").and_then(|d| d.as_array()) {
        documents.extend(many.iter().cloned());
    }
    if let (Some(index), Some(id)) =
        (spec.get("index").and_then(|v| v.as_str()), spec.get("id").and_then(|v| v.as_str()))
        && let Some(st) = store.get(index)
    {
        let g = st.read();
        let searcher = g.reader.searcher();
        let probe = boostcore::query::TermQuery::new(
            boostcore::Term::from_field_text(g.fields.id, id),
            boostcore::schema::IndexRecordOption::Basic,
        );
        if let Ok(hits) =
            searcher.search(&probe, &boostcore::collector::TopDocs::with_limit(1).order_by_score())
            && let Some((_, addr)) = hits.first()
            && let Some((_, source)) = source_of(&searcher, &g, *addr)
        {
            documents.push(source);
        }
    }
    let matched = percolated(store, targets, &field, &documents);
    o.remove("percolate");
    *node = json!({"ids": {"values": matched}});
}

/// The ids of the stored queries under `field` that any of the documents
/// matches.
fn percolated(store: &Store, targets: &[String], field: &str, documents: &[Value]) -> Vec<String> {
    if documents.is_empty() {
        return Vec::new();
    }
    // the documents live in a scratch index mapped the way the queries'
    // index is, less the field that holds the queries themselves
    let scratch = Store::new();
    let Ok(st) = scratch.ensure("_percolate") else { return Vec::new() };
    if let Some(named) = targets.first().and_then(|n| store.get(n)) {
        let mut raw = named.read().mapping.raw.clone();
        if let Some(props) = raw.get_mut("properties").and_then(|p| p.as_object_mut()) {
            props.remove(field);
        }
        let mut g = st.write();
        g.mapping = crate::store::Mapping::from_body(&raw);
        g.apply_analysis();
    }
    {
        let mut g = st.write();
        for (at, document) in documents.iter().enumerate() {
            let _ =
                crate::api::write_doc_raw(&mut g, &at.to_string(), document.clone(), "index", None);
        }
        let _ = g.refresh();
    }
    // every stored query, run over the scratch index; a query is an object
    // that may index nothing at all, so the documents are read rather than
    // asked for by the field
    let probe = json!({"query": {"match_all": {}}, "size": 10_000, "_source": [field]});
    let Ok(found) = run(store, &targets.join(","), &probe, &Params::new()) else {
        return Vec::new();
    };
    let mut matched = Vec::new();
    for hit in found.hits {
        let Some(id) = hit.get("_id").and_then(|v| v.as_str()) else { continue };
        let Some(stored) = hit.pointer(&format!("/_source/{}", field.replace('.', "/"))) else {
            continue;
        };
        let asked = json!({"query": stored, "size": 0, "track_total_hits": true});
        if let Ok(out) = run(&scratch, "_percolate", &asked, &Params::new())
            && out.total > 0
        {
            matched.push(id.to_string());
        }
    }
    matched
}

/// Whether the query walks a percolator.
pub(crate) fn names_a_percolate(node: &Value) -> bool {
    match node {
        Value::Object(o) => o.contains_key("percolate") || o.values().any(names_a_percolate),
        Value::Array(items) => items.iter().any(names_a_percolate),
        _ => false,
    }
}

/// What a query stored in a percolator field asks of fields nobody mapped.
///
/// A query is checked when it is stored, since running it later against a
/// document would fail where a search would fail: a query string that names
/// a field the mapping does not know is refused.
pub(crate) fn percolator_complaint(g: &IdxState, source: &Value) -> Option<String> {
    if !g.mapping.has_percolator() {
        return None;
    }
    for (path, kind) in g.mapping.types.iter() {
        if kind != "percolator" {
            continue;
        }
        let Some(stored) = source.pointer(&format!("/{}", path.replace('.', "/"))) else {
            continue;
        };
        if let Some(named) = unmapped_in_query(g, stored) {
            return Some(format!(
                "No field mapping can be found for the field with name [{named}]"
            ));
        }
    }
    None
}

/// The first field a query string names that the mapping does not know.
fn unmapped_in_query(g: &IdxState, query: &Value) -> Option<String> {
    match query {
        Value::Object(o) => {
            if let Some(text) =
                o.get("query_string").and_then(|q| q.get("query")).and_then(|v| v.as_str())
            {
                // `field:value`, with or without a space after the colon
                let pairs = regex::Regex::new(r"([A-Za-z_][\w.]*)\s*:\s*(\S*)").ok();
                let found: Vec<(String, String)> = pairs
                    .map(|re| {
                        re.captures_iter(text)
                            .map(|c| (c[1].to_string(), c[2].to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                for (field, value) in found {
                    let named = match field.as_str() {
                        "_exists_" => value.trim().to_string(),
                        other => other.to_string(),
                    };
                    if named.is_empty() || named == "*" {
                        continue;
                    }
                    let known = g.mapping.type_of(&named).is_some()
                        || g.mapping.types.keys().any(|k| k.starts_with(&format!("{named}.")));
                    if !known {
                        return Some(named);
                    }
                }
            }
            o.values().find_map(|v| unmapped_in_query(g, v))
        }
        Value::Array(items) => items.iter().find_map(|v| unmapped_in_query(g, v)),
        _ => None,
    }
}
