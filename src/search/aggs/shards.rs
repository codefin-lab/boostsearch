//! The documents of a search the way OpenSearch's shards see them.
//!
//! An index here is one VeloCore index whatever `number_of_shards` says; a
//! shard is only where a document's routing lands it. Most aggregations do not
//! care, but some answer differently depending on which documents a shard held
//! and in which order it read them: a terms aggregation cuts each shard's list
//! before merging them and says how far that may have put its counts out, a
//! variable-width histogram clusters values in the order they arrive, and a
//! t-digest is a different sketch when the values are added in another order.
//! Those read the documents through here.

use super::*;
use crate::search::*;

/// One value a document holds for a field.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Held {
    Number(f64),
    Text(String),
}

/// One shard's documents that a query matched, in the order the shard holds
/// them, each with the values it holds for the fields asked about.
pub(crate) struct ShardDocs {
    pub(crate) docs: Vec<(String, Vec<Vec<Held>>)>,
}

/// A document as it is read, before it is put in its shard's order: its
/// sequence number, where it sits in the index, its id and its values.
type Row = (u64, u32, u32, String, Vec<Vec<Held>>);

/// Read the matching documents of every target, shard by shard.
///
/// A shard holds its documents in the order they were written to it, which is
/// the order of their sequence numbers; that is the order returned.
pub(crate) fn shard_docs(
    store: &Store,
    targets: &[String],
    query_json: &Value,
    fields: &[&str],
) -> std::result::Result<Vec<ShardDocs>, Response> {
    let mut out = Vec::new();
    for name in targets {
        let Some(st) = store.get(name) else { continue };
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
        // narrowed the way `search_one_shard` narrows the search it stands in for
        let (narrowed, _) = crate::security::view::narrowed_for(store, name, &g, query_json, &None);
        let q = crate::query::build(&ctx, &narrowed)
            .map_err(|e| err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string()))?;
        let columns: Vec<String> = fields.iter().map(|f| ctx.column_name(f, false)).collect();
        let searcher = g.reader.searcher();
        let addrs = searcher.search(&q, &velocore::collector::DocSetCollector).map_err(|e| {
            err(StatusCode::BAD_REQUEST, "search_phase_execution_exception", e.to_string())
        })?;
        struct Segment {
            ids: Option<velocore::columnar::StrColumn>,
            seq: Option<velocore::columnar::Column<u64>>,
            values: Vec<SortColumns>,
        }
        let segments: Vec<Segment> = searcher
            .segment_readers()
            .iter()
            .map(|r| Segment {
                ids: r.fast_fields().str("_id").ok().flatten(),
                seq: r.fast_fields().u64("_seq").ok(),
                values: columns.iter().map(|c| SortColumns::for_segment(r, c)).collect(),
            })
            .collect();
        let shards = g.shard_count().max(1) as usize;
        let mut rows: Vec<Vec<Row>> = vec![Vec::new(); shards];
        let mut buf = Vec::new();
        for addr in addrs {
            let Some(seg) = segments.get(addr.segment_ord as usize) else { continue };
            let id = seg
                .ids
                .as_ref()
                .and_then(|c| c.term_ords(addr.doc_id).next().map(|o| (c, o)))
                .and_then(|(c, o)| {
                    buf.clear();
                    c.ord_to_bytes(o, &mut buf).ok()?;
                    String::from_utf8(buf.clone()).ok()
                })
                .unwrap_or_default();
            let seq = seg.seq.as_ref().and_then(|c| c.first(addr.doc_id)).unwrap_or(u64::MAX);
            let values = seg.values.iter().map(|c| c.held_values(addr.doc_id)).collect();
            let shard = (g.shard_of_doc(&id) as usize).min(shards - 1);
            rows[shard].push((seq, addr.segment_ord, addr.doc_id, id, values));
        }
        for mut docs in rows {
            docs.sort_by_key(|a| (a.0, a.1, a.2));
            out.push(ShardDocs {
                docs: docs.into_iter().map(|(_, _, _, id, v)| (id, v)).collect(),
            });
        }
    }
    Ok(out)
}

impl SortColumns {
    /// Every value a document holds in this column, numbers or text.
    fn held_values(&self, doc: velocore::DocId) -> Vec<Held> {
        let numbers = self.numeric_values(doc);
        if !numbers.is_empty() {
            return numbers.into_iter().map(Held::Number).collect();
        }
        let Some((Some(col), _, _)) = self.per_segment.first() else { return Vec::new() };
        let mut out = Vec::new();
        let mut buf = Vec::new();
        for ord in col.term_ords(doc) {
            buf.clear();
            if col.ord_to_bytes(ord, &mut buf).unwrap_or(false)
                && let Ok(s) = String::from_utf8(buf.clone())
            {
                out.push(Held::Text(s));
            }
        }
        out
    }
}

/// How a terms aggregation is ordered, as far as its error bound cares.
#[derive(PartialEq)]
enum TermsOrder {
    CountDesc,
    Key,
    Other,
}

fn terms_order(spec: &Value) -> TermsOrder {
    let first = match spec.get("order") {
        Some(Value::Array(list)) => list.first().cloned(),
        Some(other) => Some(other.clone()),
        None => None,
    };
    let Some((key, dir)) = first
        .as_ref()
        .and_then(|o| o.as_object())
        .and_then(|o| o.iter().next())
        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("asc").to_string()))
    else {
        return TermsOrder::CountDesc;
    };
    match (key.as_str(), dir.as_str()) {
        ("_count", "desc") => TermsOrder::CountDesc,
        ("_key" | "_term", _) => TermsOrder::Key,
        _ => TermsOrder::Other,
    }
}

/// A key the way the merge compares it: numbers by value, text by its bytes.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
enum KeyOf {
    Number(i64),
    Text(String),
}

impl KeyOf {
    fn of(held: &Held) -> KeyOf {
        match held {
            // ordered by value: the bits of a double, bent the way
            // `f64::total_cmp` bends them, sort the way the double does
            Held::Number(n) => {
                let n = if *n == 0.0 { 0.0 } else { *n };
                let bits = n.to_bits() as i64;
                KeyOf::Number(bits ^ (((bits >> 63) as u64) >> 1) as i64)
            }
            Held::Text(s) => KeyOf::Text(s.clone()),
        }
    }

    fn of_json(v: &Value) -> Option<KeyOf> {
        match v {
            Value::String(s) => Some(KeyOf::Text(s.clone())),
            Value::Number(n) => n.as_f64().map(|f| KeyOf::of(&Held::Number(f))),
            _ => None,
        }
    }
}

/// Put right what a terms aggregation says about how exact it is.
///
/// A shard of OpenSearch keeps only its top `shard_size` terms and the merge
/// adds up what the shards kept, so a count can come out short, and the answer
/// says by how much it may be: `doc_count_error_upper_bound`, overall and with
/// `show_term_doc_count_error` for each bucket. One shard has nothing to merge,
/// and its bound is 0. VeloCore counts over segments rather than shards, so it
/// reported a bound of 146 for a single-shard index where the reference
/// reports 0, and over three shards counts that no shard had cut. The shards
/// are read here, each cut the way the reference cuts it, and the counts, the
/// bounds and `sum_other_doc_count` are the ones that merge makes.
pub(crate) fn shard_terms_bounds(
    store: &Store,
    targets: &[String],
    base: &Value,
    request: &Value,
    answer: &mut Value,
) -> std::result::Result<(), Response> {
    let Some(reqs) = request.as_object() else { return Ok(()) };
    for (name, def) in reqs {
        let Some(node) = answer.get_mut(name) else { continue };
        if let Some(spec) = def.get("terms")
            && spec.get("script").is_none()
            && spec.get("field").and_then(|f| f.as_str()).map(|f| f != "_index").unwrap_or(false)
        {
            settle_terms(store, targets, base, def, spec, node)?;
        }
        let Some(subs) = def.get("aggs").or_else(|| def.get("aggregations")) else { continue };
        let descend = |bucket: &mut Value, narrowing: Option<Value>| {
            let Some(filter) = narrowing else { return Ok(()) };
            let narrowed = json!({"bool": {"filter": [base.clone(), filter]}});
            shard_terms_bounds(store, targets, &narrowed, subs, bucket)
        };
        match node.get_mut("buckets") {
            Some(Value::Array(list)) => {
                for b in list.iter_mut() {
                    let filter = bucket_filter(store, targets, def, b);
                    descend(b, filter)?;
                }
            }
            Some(Value::Object(_)) => {}
            _ => {
                if let Some(filter) = def.get("filter") {
                    descend(node, Some(filter.clone()))?;
                }
            }
        }
    }
    Ok(())
}

fn settle_terms(
    store: &Store,
    targets: &[String],
    base: &Value,
    def: &Value,
    spec: &Value,
    node: &mut Value,
) -> std::result::Result<(), Response> {
    let Some(buckets) = node.get("buckets").and_then(|b| b.as_array()) else { return Ok(()) };
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or_default().to_string();
    let order = terms_order(spec);
    let show = spec.get("show_term_doc_count_error").and_then(|v| v.as_bool()) == Some(true);
    let size = spec.get("size").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
    // a shard keeps half as many again as the answer shows, and ten more,
    // unless told otherwise -- and never fewer than the answer shows
    let shard_size = spec
        .get("shard_size")
        .and_then(|v| v.as_u64())
        .map(|s| s as usize)
        .unwrap_or_else(|| (size as f64 * 1.5 + 10.0) as usize)
        .max(size);
    let shards: u64 =
        targets.iter().filter_map(|n| store.get(n)).map(|st| st.read().shard_count().max(1)).sum();
    let set_bounds = |node: &mut Value, overall: i64, per: &dyn Fn(&Value) -> i64| {
        node["doc_count_error_upper_bound"] = json!(overall);
        if show && let Some(list) = node.get_mut("buckets").and_then(|b| b.as_array_mut()) {
            for b in list.iter_mut() {
                let e = per(b);
                b["doc_count_error_upper_bound"] = json!(e);
            }
        }
    };
    if order == TermsOrder::Key {
        set_bounds(node, 0, &|_| 0);
        return Ok(());
    }
    let other = node.get("sum_other_doc_count").and_then(|v| v.as_u64()).unwrap_or(0);
    // Nothing a shard could have cut: every term is in the answer and there
    // are fewer of them than a shard keeps, so no shard kept a full list.
    if other == 0 && buckets.len() < shard_size {
        set_bounds(node, 0, &|_| 0);
        return Ok(());
    }
    let ty = targets
        .iter()
        .filter_map(|n| store.get(n))
        .find_map(|st| st.read().mapping.type_of(&field).map(|t| t.to_string()));
    let countable = matches!(
        ty.as_deref(),
        Some(
            "keyword"
                | "long"
                | "integer"
                | "short"
                | "byte"
                | "double"
                | "float"
                | "half_float"
                | "scaled_float"
        )
    );
    // a single shard merges nothing, and the only bound it can give is none
    // at all, which is what an order the counts cannot bound says
    if shards == 1 || !countable || spec.get("include").is_some() || spec.get("exclude").is_some() {
        match (order, shards) {
            (TermsOrder::CountDesc, 1) => set_bounds(node, 0, &|_| 0),
            (TermsOrder::Other, 1) if countable => {
                let docs = shard_docs(store, targets, base, &[&field])?;
                let distinct = docs
                    .first()
                    .map(|s| {
                        let mut keys: Vec<KeyOf> =
                            s.docs.iter().flat_map(|(_, v)| v[0].iter().map(KeyOf::of)).collect();
                        keys.sort();
                        keys.dedup();
                        keys.len()
                    })
                    .unwrap_or(0);
                let e = if distinct >= shard_size { -1 } else { 0 };
                set_bounds(node, e, &|_| e);
            }
            _ => {}
        }
        return Ok(());
    }
    let missing = spec.get("missing").and_then(|m| match m {
        Value::String(s) => Some(Held::Text(s.clone())),
        Value::Number(n) => n.as_f64().map(Held::Number),
        _ => None,
    });
    let docs = shard_docs(store, targets, base, &[&field])?;
    // each shard's own count of each term, and what it keeps of them
    let mut sum_error: i64 = 0;
    let mut other_docs: u64 = 0;
    let mut kept: Vec<(std::collections::HashMap<KeyOf, u64>, i64)> = Vec::new();
    for shard in &docs {
        let mut counts: std::collections::HashMap<KeyOf, u64> = Default::default();
        let mut total = 0u64;
        for (_, values) in &shard.docs {
            let mut held: Vec<KeyOf> = values[0].iter().map(KeyOf::of).collect();
            if held.is_empty()
                && let Some(m) = missing.as_ref()
            {
                held.push(KeyOf::of(m));
            }
            held.dedup();
            for k in held {
                *counts.entry(k).or_default() += 1;
                total += 1;
            }
        }
        let mut ranked: Vec<(KeyOf, u64)> = counts.into_iter().collect();
        match order {
            TermsOrder::CountDesc => {
                ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)))
            }
            _ => ranked.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0))),
        }
        ranked.truncate(shard_size);
        let error = if ranked.is_empty() || ranked.len() < shard_size {
            0
        } else if order == TermsOrder::CountDesc {
            ranked.iter().map(|(_, c)| *c).min().unwrap_or(0) as i64
        } else {
            -1
        };
        other_docs += total - ranked.iter().map(|(_, c)| *c).sum::<u64>();
        if sum_error != -1 {
            sum_error = if error == -1 { -1 } else { sum_error + error };
        }
        kept.push((ranked.into_iter().collect(), error));
    }
    if order == TermsOrder::Other {
        // the counts a sub-aggregation or an ascending order ranks are not
        // simulated; only the bound it gives is
        let e = sum_error;
        set_bounds(node, e, &|_| e);
        return Ok(());
    }
    // what the merge adds up, and the error each term carries: the errors of
    // the shards that did not keep it
    let mut merged: std::collections::HashMap<KeyOf, (u64, i64)> = Default::default();
    for (counts, error) in &kept {
        for (k, c) in counts {
            let e = merged.entry(k.clone()).or_insert((0, 0));
            e.0 += c;
            e.1 += error;
        }
    }
    let min_doc_count = spec.get("min_doc_count").and_then(|v| v.as_u64()).unwrap_or(1);
    let mut ranked: Vec<(KeyOf, u64, i64)> = merged
        .into_iter()
        .filter(|(_, (c, _))| *c >= min_doc_count)
        .map(|(k, (c, kept_error))| (k, c, sum_error - kept_error))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let cut: u64 = ranked.iter().skip(size).map(|(_, c, _)| *c).sum();
    ranked.truncate(size);
    let existing: Vec<Value> = buckets.clone();
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let mut rebuilt = Vec::new();
    for (key, count, error) in &ranked {
        let found =
            existing.iter().find(|b| b.get("key").and_then(KeyOf::of_json).as_ref() == Some(key));
        let mut b = match found {
            Some(b) => b.clone(),
            None => {
                let key_json = match key {
                    KeyOf::Text(s) => json!(s),
                    KeyOf::Number(_) => {
                        // the key is found again from the documents themselves
                        let Some(n) = docs
                            .iter()
                            .flat_map(|s| s.docs.iter())
                            .flat_map(|(_, v)| v[0].iter())
                            .find_map(|h| match h {
                                Held::Number(n) if KeyOf::of(h) == *key => Some(*n),
                                _ => None,
                            })
                        else {
                            continue;
                        };
                        match ty.as_deref() {
                            Some("long" | "integer" | "short" | "byte") => json!(n as i64),
                            _ => json!(n),
                        }
                    }
                };
                json!({"key": key_json, "doc_count": count})
            }
        };
        let short = b.get("doc_count").and_then(|c| c.as_u64()) != Some(*count);
        b["doc_count"] = json!(count);
        // a count the merge left short is short in what is under the bucket
        // too: only the shards that kept the term took part in it
        if short && let Some(subs) = sub_aggs.as_ref() {
            let Some(filter) = bucket_filter(store, targets, def, &b) else { continue };
            let ids: Vec<String> = docs
                .iter()
                .zip(kept.iter())
                .filter(|(_, (counts, _))| counts.contains_key(key))
                .flat_map(|(s, _)| s.docs.iter().map(|(id, _)| id.clone()))
                .collect();
            let narrowed =
                json!({"bool": {"filter": [base.clone(), filter, {"ids": {"values": ids}}]}});
            let (_, sub) =
                count_with_sub_aggs(store, targets, &narrowed, &Some(subs.clone()), false)?;
            if let Some(Value::Object(o)) = sub {
                for (k, v) in o {
                    b[k] = v;
                }
            }
        }
        if show {
            b["doc_count_error_upper_bound"] = json!(if sum_error == -1 { -1 } else { *error });
        }
        rebuilt.push(b);
    }
    node["buckets"] = Value::Array(rebuilt);
    node["sum_other_doc_count"] = json!(other_docs + cut);
    node["doc_count_error_upper_bound"] = json!(sum_error);
    Ok(())
}
