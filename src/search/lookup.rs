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
/// The most values a bitmap may be unpacked into.
///
/// A roaring bitmap is compact in a way that matters here: four bytes of run
/// container stand for 65,536 ids, so a request body of a few kilobytes
/// unpacks into hundreds of millions of `i64`s -- gigabytes of them, built
/// before anything looked at `index.max_terms_count`. The decoders stop at
/// this many and answer nothing, and the caller is told the list is too long.
pub(crate) const MOST_BITMAP_VALUES: usize = 1_048_576;

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
        if out.len() > MOST_BITMAP_VALUES {
            return None;
        }
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
    // a count the bytes cannot hold is not a count: every container costs at
    // least its key and its cardinality, so the bytes bound what to allocate
    if count > bytes.len().saturating_sub(at) / 4 {
        return None;
    }
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
    // a container may not be unpacked at all if what is already unpacked plus
    // what this one holds is past the ceiling: the check is before the work,
    // not after it
    let room = |so_far: usize, more: usize| -> Option<()> {
        if so_far + more > MOST_BITMAP_VALUES { None } else { Some(()) }
    };
    for (i, (key, card)) in keys.iter().enumerate() {
        let high = (*key as i64) << 16;
        if runs[i] {
            let n = u16_at(at)? as usize;
            at += 2;
            for _ in 0..n {
                let start = u16_at(at)? as i64;
                let len = u16_at(at + 2)? as i64;
                at += 4;
                room(out.len(), len as usize + 1)?;
                for v in start..=start + len {
                    out.push(high | v);
                }
            }
        } else if *card <= 4096 {
            room(out.len(), *card as usize)?;
            for _ in 0..*card {
                out.push(high | u16_at(at)? as i64);
                at += 2;
            }
        } else {
            room(out.len(), *card as usize)?;
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
pub(crate) fn expand_bitmap_terms(node: &mut Value) -> std::result::Result<(), Response> {
    let Some(o) = node.as_object_mut() else { return Ok(()) };
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
            let too_long = || {
                err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!(
                        "The bitmap in [terms] query for field [{f}] could not be read, or \
                         holds more than {MOST_BITMAP_VALUES} values."
                    ),
                )
            };
            // the 32-bit form starts with its cookie; the 64-bit form
            // starts with a count of the high words it groups by
            let Some(values) = base64_decode(&encoded)
                .as_deref()
                .and_then(|b| decode_roaring(b).or_else(|| decode_roaring64(b)))
            else {
                // it used to be left as it arrived, and a base64 string was
                // then asked for as if it were the term itself
                return Err(too_long());
            };
            terms.insert(f, Value::Array(values.into_iter().map(|v| json!(v)).collect()));
        }
    }
    for (_, v) in o.iter_mut() {
        match v {
            Value::Object(_) => expand_bitmap_terms(v)?,
            Value::Array(a) => {
                for item in a.iter_mut() {
                    expand_bitmap_terms(item)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
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
/// OpenSearch picks the terms the way its `XMoreLikeThis` does, and this
/// follows it step by step. The words of what is liked are counted: a text
/// is cut by the search analyzer of the first field named, a document gives
/// the terms its fields were indexed as. Each word that is frequent enough
/// there, held by enough documents and not by too many, is scored `tf * idf`
/// with the classic idf, and the best `max_query_terms` of them become term
/// queries -- chosen separately for each field of the liked documents, and
/// once over all the fields for the liked texts, where each word goes to the
/// field that holds it most. `minimum_should_match` (30% unless asked) is
/// then counted over those term queries, and the liked documents themselves
/// are left out unless `include` says otherwise. `unlike` names words to
/// leave out of the counting altogether.
///
/// It used to split the text on spaces, keep every word, ask for any one of
/// them and read none of `max_doc_freq`, `stop_words`, the word lengths or
/// `minimum_should_match`, which found nearly every document; and a field
/// such as `body.english` is not a key of the source, so it found none.
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
    // the statistics are the shard's, and this node answers an index as one
    let Some(st) = targets.first().and_then(|n| store.get(n)) else { return };
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
    let searcher = g.reader.searcher();

    let int = |key: &str, default: i64| spec.get(key).and_then(|v| v.as_i64()).unwrap_or(default);
    let max_query_terms = int("max_query_terms", 25).max(1) as usize;
    let min_term_freq = int("min_term_freq", 2);
    let min_doc_freq = int("min_doc_freq", 5);
    let max_doc_freq = int("max_doc_freq", i32::MAX as i64);
    let min_word_length = int("min_word_length", 0);
    let max_word_length = int("max_word_length", 0);
    let boost_terms = spec.get("boost_terms").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
    let include = spec.get("include").and_then(|v| v.as_bool()).unwrap_or(false);
    let msm = match spec.get("minimum_should_match") {
        Some(Value::String(s)) => s.clone(),
        Some(other) if !other.is_null() => other.to_string(),
        _ => "30%".to_string(),
    };
    let stop_words: Option<std::collections::HashSet<String>> = spec
        .get("stop_words")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|w| w.as_str().map(|s| s.to_string())).collect());
    let analyzer = spec.get("analyzer").and_then(|v| v.as_str());

    // `like` and `unlike` each hold texts and documents, one or a list
    let listed = |key: &str| -> (Vec<String>, Vec<Value>) {
        let items: Vec<Value> = match spec.get(key) {
            Some(Value::Array(a)) => a.clone(),
            Some(one) => vec![one.clone()],
            None => Vec::new(),
        };
        let texts = items.iter().filter_map(|i| i.as_str().map(|s| s.to_string())).collect();
        (texts, items.into_iter().filter(|i| i.is_object()).collect())
    };
    let (like_texts, like_items) = listed("like");
    let (unlike_texts, unlike_items) = listed("unlike");

    // the fields: those named that can be read as words, or every such field
    let readable = |name: &str| matches!(g.mapping.type_of(name), Some("text" | "keyword"));
    let every_field = || -> Vec<String> {
        let mut all: Vec<String> = g
            .mapping
            .types
            .keys()
            .filter(|name| !name.starts_with('_') && readable(name))
            .cloned()
            .collect();
        all.sort();
        all
    };
    let named: Option<Vec<String>> = spec.get("fields").and_then(|f| f.as_array()).map(|a| {
        a.iter()
            .filter_map(|x| x.as_str().map(|s| s.to_string()))
            .filter(|name| g.mapping.type_of(name).is_none() || readable(name))
            .collect()
    });
    let fields: Vec<String> = match &named {
        Some(named) => named.clone(),
        None => vec!["*".to_string()],
    };
    if fields.is_empty() {
        *node = json!({"match_none": {}});
        return;
    }

    let noise = |word: &str| {
        let len = word.encode_utf16().count() as i64;
        (min_word_length > 0 && len < min_word_length)
            || (max_word_length > 0 && len > max_word_length)
            || stop_words.as_ref().is_some_and(|s| s.contains(word))
    };
    // the words a text is cut into, by the analyzer of the field it is read for
    let cut = |field: &str, text: &str| -> Vec<String> {
        let (_, _, view) = ctx.resolve(field, true);
        crate::query::analyze_with(&ctx, view, field, text, analyzer)
    };
    // the terms of a document, field by field, as its term vectors give them
    let terms_of = |item: &Value| -> Option<DocTerms> {
        let (source, wanted): (Value, Option<Vec<String>>) = match item.get("doc") {
            Some(doc) => (doc.clone(), None),
            None => {
                let id = match item.get("_id")? {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let index = item.get("_index").and_then(|v| v.as_str());
                let source = match index {
                    Some(name) if name != g.name => {
                        let other = store.get(name)?;
                        let other = other.read();
                        crate::api::read_source(&other, &id)?
                    }
                    _ => crate::api::read_source(&g, &id)?,
                };
                (source, Some(fields.clone()))
            }
        };
        let wanted: Vec<String> = match item.get("fields").and_then(|f| f.as_array()) {
            Some(own) => own.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect(),
            None => match wanted {
                Some(w) if !w.iter().any(|f| f == "*") => w,
                _ => every_field(),
            },
        };
        let shape = crate::api::Shape {
            term_statistics: false,
            field_statistics: false,
            positions: false,
            offsets: false,
        };
        let vectors = crate::api::term_vectors_of(&g, &source, shape, Some(&wanted));
        let mut out = Vec::new();
        for (field, body) in vectors.as_object()? {
            let terms = body
                .get("terms")
                .and_then(|t| t.as_object())
                .map(|t| {
                    t.iter()
                        .map(|(term, v)| {
                            (
                                term.clone(),
                                v.get("term_freq").and_then(|f| f.as_u64()).unwrap_or(0) as u32,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.push((field.clone(), terms));
        }
        Some(out)
    };

    // words to leave out, by field
    let mut skip: std::collections::HashSet<(String, String)> = Default::default();
    for text in &unlike_texts {
        for word in cut(&fields[0], text) {
            skip.insert((fields[0].clone(), word));
        }
    }
    let liked: Vec<DocTerms> = like_items.iter().filter_map(terms_of).collect();
    if !like_items.is_empty() {
        for item in &unlike_items {
            for (field, terms) in terms_of(item).unwrap_or_default() {
                for (term, _) in terms {
                    skip.insert((field.clone(), term));
                }
            }
        }
    }

    let doc_freq = |field: &str, word: &str| -> u64 {
        let (f, path, _) = ctx.resolve(field, false);
        crate::query::term_for(f, &path, &json!(word))
            .first()
            .map(|t| searcher.doc_freq(t).unwrap_or(0))
            .unwrap_or(0)
    };
    let num_docs = searcher.num_docs();
    // the best-scoring words of a count, as term queries, lowest score first
    let choose = |counts: &JavaCounts, fields: &[String]| -> Vec<Value> {
        let words = counts.in_java_order();
        let limit = max_query_terms.min(words.len());
        let mut chosen: Vec<(String, String, f32)> = Vec::new();
        let mut heap = crate::query::positions::LuceneHeap::new();
        for (word, tf) in words {
            if min_term_freq > 0 && (tf as i64) < min_term_freq {
                continue;
            }
            let mut top_field = fields[0].clone();
            let mut df = 0u64;
            for field in fields {
                let freq = doc_freq(field, &word);
                if freq > df {
                    top_field = field.clone();
                    df = freq;
                }
            }
            if (min_doc_freq > 0 && (df as i64) < min_doc_freq)
                || df as i64 > max_doc_freq
                || df == 0
            {
                continue;
            }
            let idf = (((num_docs + 1) as f64 / (df + 1) as f64).ln() + 1.0) as f32;
            let score = tf as f32 * idf;
            if heap.len() < limit {
                chosen.push((word, top_field, score));
                let at = chosen.len() - 1;
                let scores: Vec<f32> = chosen.iter().map(|c| c.2).collect();
                heap.add(at, &|a, b| scores[a] < scores[b]);
            } else if let Some(top) = heap.top()
                && chosen[top].2 < score
            {
                chosen[top] = (word, top_field, score);
                let scores: Vec<f32> = chosen.iter().map(|c| c.2).collect();
                heap.update_top(&|a, b| scores[a] < scores[b]);
            }
        }
        let scores: Vec<f32> = chosen.iter().map(|c| c.2).collect();
        let mut clauses = Vec::new();
        let mut lowest = -1f32;
        while let Some(at) = heap.pop(&|a, b| scores[a] < scores[b]) {
            let (word, field, score) = &chosen[at];
            let mut term = json!({"value": word});
            if boost_terms != 0.0 {
                if lowest == -1.0 {
                    lowest = *score;
                }
                term["boost"] = json!(boost_terms * score / lowest);
            }
            // The reference's clause is a bare Lucene term query, scored by
            // BM25 on every field. A `term` on a keyword scores one, as the
            // reference's own `term` query does; a `span_term` is the same
            // bare term, scored.
            let kind = match ctx.resolve(field, true).2 {
                crate::query::View::Raw => "span_term",
                _ => "term",
            };
            clauses.push(json!({kind: {field.clone(): term}}));
        }
        clauses
    };
    let with_msm = |clauses: Vec<Value>| -> Value {
        // a Lucene boolean query of no clauses matches nothing, where an
        // empty `bool` here would match everything
        if clauses.is_empty() {
            return json!({"match_none": {}});
        }
        let required = min_should_match(clauses.len(), &msm);
        let mut bool_q = json!({"should": clauses});
        if required > 0 {
            bool_q["minimum_should_match"] = json!(required);
        }
        json!({"bool": bool_q})
    };

    let mut parts = Vec::new();
    if !like_items.is_empty() {
        // each field of the liked documents chooses its own words
        let mut names: Vec<String> = Vec::new();
        for doc in &liked {
            for (field, _) in doc {
                if !names.contains(field) {
                    names.push(field.clone());
                }
            }
        }
        let mut clauses = Vec::new();
        for name in &names {
            let mut counts = JavaCounts::default();
            for doc in &liked {
                for (field, terms) in doc {
                    if field != name {
                        continue;
                    }
                    for (term, freq) in terms {
                        if noise(term) || skip.contains(&(name.clone(), term.clone())) {
                            continue;
                        }
                        counts.add(term, *freq);
                    }
                }
            }
            clauses.extend(choose(&counts, std::slice::from_ref(name)));
        }
        parts.push(with_msm(clauses));
    }
    if !like_texts.is_empty() {
        if named.is_none() {
            // there is no field to cut a text by
            *node = json!({"match_none": {}});
            return;
        }
        let mut counts = JavaCounts::default();
        for text in &like_texts {
            for (at, word) in cut(&fields[0], text).into_iter().enumerate() {
                if at >= 5000 {
                    break;
                }
                if noise(&word) || skip.contains(&(fields[0].clone(), word.clone())) {
                    continue;
                }
                counts.add(&word, 1);
            }
        }
        parts.push(with_msm(choose(&counts, &fields)));
    }
    let mut query = json!({"bool": {"should": parts}});
    if !like_items.is_empty() && !include {
        let ids: Vec<Value> = like_items
            .iter()
            .filter(|item| item.get("doc").is_none())
            .filter_map(|item| {
                item.get("_id").map(|id| match id {
                    Value::String(s) => json!(s),
                    other => json!(other.to_string()),
                })
            })
            .collect();
        if !ids.is_empty() {
            query = json!({"bool": {"should": [query], "must_not": [{"ids": {"values": ids}}]}});
        }
    }
    o.remove("more_like_this");
    *node = query;
}

/// How many of `n` optional clauses `spec` asks for, counted as OpenSearch's
/// `Queries.calculateMinShouldMatch` counts: a percentage is taken of the
/// count in float and cut down, and nothing caps it at `n`.
fn min_should_match(n: usize, spec: &str) -> usize {
    let spec = spec.trim();
    let n = n as i32;
    if spec.contains('<') {
        let mut result = n;
        let spaced = spec.split_whitespace().collect::<Vec<_>>().join(" ");
        let tight = spaced.replace(" < ", "<").replace(" <", "<").replace("< ", "<");
        for part in tight.split(' ') {
            let Some((bound, rule)) = part.split_once('<') else { continue };
            let Ok(bound) = bound.parse::<i32>() else { continue };
            if n <= bound {
                return result.max(0) as usize;
            }
            result = min_should_match(n as usize, rule) as i32;
        }
        return result.max(0) as usize;
    }
    let result = if let Some(percent) = spec.strip_suffix('%') {
        let percent: i32 = percent.trim().parse().unwrap_or(0);
        let calc = (n.wrapping_mul(percent)) as f32 * 0.01f32;
        if calc < 0.0 { n + calc as i32 } else { calc as i32 }
    } else {
        let calc: i32 = spec.parse().unwrap_or(0);
        if calc < 0 { n + calc } else { calc }
    };
    result.max(0) as usize
}

/// Word counts kept the way a Java `HashMap<String, _>` keeps them.
///
/// Which words make the cut when two score the same depends on which one the
/// reference meets first, and it meets them in its hash map's order: by
/// bucket of the string's hash, then in the order they went in. Walking them
/// in any other order keeps a different word of a tie.
/// The terms of one liked document, field by field, each with how often it
/// stands there.
type DocTerms = Vec<(String, Vec<(String, u32)>)>;

#[derive(Default)]
struct JavaCounts {
    words: Vec<(String, u32)>,
}

impl JavaCounts {
    fn add(&mut self, word: &str, freq: u32) {
        match self.words.iter_mut().find(|(w, _)| w == word) {
            Some((_, count)) => *count += freq,
            None => self.words.push((word.to_string(), freq)),
        }
    }

    fn in_java_order(&self) -> Vec<(String, u32)> {
        let mut capacity = 16usize;
        while self.words.len() > capacity * 3 / 4 {
            capacity *= 2;
        }
        let bucket = |word: &str| {
            let mut h: i32 = 0;
            for unit in word.encode_utf16() {
                h = h.wrapping_mul(31).wrapping_add(unit as i32);
            }
            let spread = (h ^ ((h as u32) >> 16) as i32) as u32;
            spread as usize & (capacity - 1)
        };
        let mut ordered = self.words.clone();
        ordered.sort_by_key(|(word, _)| bucket(word));
        ordered
    }
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
                    // A terms lookup reads a document out of an index the
                    // caller named, which is not the index being searched:
                    // the layer judged that one and nothing judged this. The
                    // values become the terms of a query, so what comes back
                    // says what the document holds.
                    if let Some(why) = crate::security::item_refusal(
                        store,
                        &["indices:data/read/get"],
                        &[index.to_string()],
                    ) {
                        return Err(crate::api::err(
                            axum::http::StatusCode::FORBIDDEN,
                            "security_exception",
                            why,
                        ));
                    }
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
                        // a document the caller's own filter hides, or a
                        // field it hides, is not a document this may read
                        let values = crate::api::read_source(&g, id)
                            .filter(|_| crate::security::doc_visible(store, &g, id))
                            .map(|mut src| {
                                crate::security::narrow_source(store, &g.name, &mut src);
                                src
                            })
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
            let score_mode = spec.get("score_mode").and_then(|v| v.as_str()).unwrap_or("none");
            let least = spec.get("min_children").and_then(|v| v.as_u64()).unwrap_or(1);
            let most = spec.get("max_children").and_then(|v| v.as_u64());
            if score_mode == "none" && least <= 1 && most.is_none() {
                let parents =
                    ids_of_field(store, targets, &of_that_kind, &format!("{field}.parent"));
                json!({"ids": {"values": parents}})
            } else {
                // How many children answer, and how well, is a question about
                // each parent. `min_children` was not read, so a parent with
                // one matching child passed a request for two; `score_mode`
                // was not read either, and every parent scored one. The
                // children are asked for directly and counted per parent.
                // A parent scores by what its children's query gives them,
                // with the join term as a filter; and a `function_score` is
                // only carried out at the top of a query, so the filter goes
                // inside it rather than it inside a `bool`.
                let side = on_that_side(&field, child);
                let scored = match inner.get("function_score") {
                    Some(Value::Object(fs)) => {
                        let mut fs = fs.clone();
                        let q = fs.remove("query").unwrap_or_else(|| json!({"match_all": {}}));
                        fs.insert("query".into(), json!({"bool": {"must": [q], "filter": [side]}}));
                        json!({ "function_score": fs })
                    }
                    _ => json!({"bool": {"must": [inner], "filter": [side]}}),
                };
                let probe = json!({
                    "query": scored,
                    "size": 10_000,
                    "_source": [format!("{field}.parent")],
                });
                let hits = run(store, &targets.join(","), &probe, &Params::new())
                    .map(|out| out.hits)
                    .unwrap_or_default();
                let mut per_parent: std::collections::BTreeMap<String, Vec<f64>> =
                    Default::default();
                for hit in &hits {
                    let parent = hit
                        .pointer(&format!("/_source/{}/parent", field.replace('.', "/")))
                        .map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        });
                    let Some(parent) = parent else { continue };
                    let score = hit.get("_score").and_then(|v| v.as_f64()).unwrap_or(1.0);
                    per_parent.entry(parent).or_default().push(score);
                }
                per_parent.retain(|_, scores| {
                    let n = scores.len() as u64;
                    n >= least && most.map(|m| n <= m).unwrap_or(true)
                });
                if score_mode == "none" {
                    let kept: Vec<&String> = per_parent.keys().collect();
                    json!({"ids": {"values": kept}})
                } else {
                    let clauses: Vec<Value> = per_parent
                        .iter()
                        .map(|(parent, scores)| {
                            let n = scores.len() as f64;
                            let score = match score_mode {
                                "max" => scores.iter().cloned().fold(f64::MIN, f64::max),
                                "min" => scores.iter().cloned().fold(f64::MAX, f64::min),
                                "sum" => scores.iter().sum(),
                                _ => scores.iter().sum::<f64>() / n,
                            };
                            json!({"constant_score": {
                                "filter": {"ids": {"values": [parent]}},
                                "boost": score,
                            }})
                        })
                        .collect();
                    if clauses.is_empty() {
                        json!({"match_none": {}})
                    } else {
                        json!({"bool": {"should": clauses, "minimum_should_match": 1}})
                    }
                }
            }
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
/// document would match, each query is run over the documents in a scratch
/// index holding nothing else, and the clause is read as the ids of the
/// queries that found them -- each scored as its query scored the documents,
/// since a stored `match` finds one document better than another and the
/// reference orders the rules that way. Which documents each rule found, and
/// the highlights, are put on the hits after the search
/// (`attach_percolate_slots`).
pub(crate) fn expand_percolate(
    store: &Store,
    targets: &[String],
    node: &mut Value,
) -> std::result::Result<(), Response> {
    let Some(o) = node.as_object_mut() else { return Ok(()) };
    for (_, v) in o.iter_mut() {
        match v {
            Value::Object(_) => expand_percolate(store, targets, v)?,
            Value::Array(a) => {
                for x in a.iter_mut() {
                    expand_percolate(store, targets, x)?;
                }
            }
            _ => {}
        }
    }
    let Some(spec) = o.get("percolate").cloned() else { return Ok(()) };
    let field = spec.get("field").and_then(|v| v.as_str()).unwrap_or("query").to_string();
    let documents = percolate_documents(store, &spec)?;
    let matched = percolated(store, targets, &field, &documents);
    let boost = spec.get("boost").and_then(|b| b.as_f64()).unwrap_or(1.0);
    let name = spec.get("_name").cloned();
    o.remove("percolate");
    // a rule found by no document is no hit; each rule found scores as its
    // query scored the best of the documents
    let should: Vec<Value> = matched
        .iter()
        .map(|m| {
            json!({"constant_score": {"filter": {"ids": {"values": [m.id]}},
                                      "boost": (m.score as f64 * boost).max(0.0)}})
        })
        .collect();
    let mut rewritten = if should.is_empty() {
        json!({"bool": {"must_not": {"match_all": {}}}})
    } else {
        json!({"bool": {"should": should, "minimum_should_match": 1}})
    };
    if let Some(name) = name {
        rewritten["bool"]["_name"] = name;
    }
    *node = rewritten;
    Ok(())
}

/// The documents a `percolate` clause names: written into it, or read out of
/// an index by id. A document that is not there is refused, as the reference
/// refuses it -- an empty answer said no rule matched a document nobody had
/// looked at.
fn percolate_documents(store: &Store, spec: &Value) -> std::result::Result<Vec<Value>, Response> {
    let mut documents: Vec<Value> = Vec::new();
    if let Some(one) = spec.get("document") {
        documents.push(one.clone());
    }
    if let Some(many) = spec.get("documents").and_then(|d| d.as_array()) {
        documents.extend(many.iter().cloned());
    }
    let (Some(index), Some(id)) =
        (spec.get("index").and_then(|v| v.as_str()), spec.get("id").and_then(|v| v.as_str()))
    else {
        return Ok(documents);
    };
    let Some(st) = store.get(index) else {
        return Err(err(
            StatusCode::NOT_FOUND,
            "index_not_found_exception",
            format!("no such index [{index}]"),
        ));
    };
    // The document is read out of an index the caller named, which is not
    // the index being searched: the layer judged the one on the path and
    // nothing judged this one. A caller could percolate a document out of
    // an index they may not read and learn its field values from which
    // queries matched.
    if crate::security::item_refusal(store, &["indices:data/read/get"], &[index.to_string()])
        .is_some()
    {
        documents.push(json!({}));
        return Ok(documents);
    }
    let g = st.read();
    let searcher = g.reader.searcher();
    let probe = boostcore::query::TermQuery::new(
        boostcore::Term::from_field_text(g.fields.id, id),
        boostcore::schema::IndexRecordOption::Basic,
    );
    let found = searcher
        .search(&probe, &boostcore::collector::TopDocs::with_limit(1).order_by_score())
        .ok()
        .and_then(|hits| hits.first().map(|(_, addr)| *addr))
        .filter(|_| crate::security::doc_visible(store, &g, id))
        .and_then(|addr| source_of(&searcher, &g, addr));
    match found {
        Some((_, mut source)) => {
            // and what is hidden from the caller cannot be percolated
            // against either
            crate::security::narrow_source(store, &g.name, &mut source);
            documents.push(source);
            Ok(documents)
        }
        None => Err(err(
            StatusCode::NOT_FOUND,
            "resource_not_found_exception",
            format!("indexed document [{index}/{id}] couldn't be found"),
        )),
    }
}

/// A stored query that found some of the documents, and how well it found
/// the best of them.
struct PercolateMatch {
    id: String,
    score: f32,
}

/// The scratch index the documents are percolated in: mapped the way the
/// queries' index is, less the field that holds the queries themselves.
fn percolate_scratch(
    store: &Store,
    targets: &[String],
    field: &str,
    documents: &[Value],
) -> Option<Store> {
    let scratch = Store::scratch();
    let st = scratch.ensure("_percolate").ok()?;
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
    Some(scratch)
}

/// Every stored query under `field` in the target indices, by id.
fn stored_queries(store: &Store, targets: &[String], field: &str) -> Vec<(String, Value)> {
    // a query is an object that may index nothing at all, so the documents
    // are read rather than asked for by the field
    let probe = json!({"query": {"match_all": {}}, "size": 10_000, "_source": [field]});
    let Ok(found) = run(store, &targets.join(","), &probe, &Params::new()) else {
        return Vec::new();
    };
    found
        .hits
        .into_iter()
        .filter_map(|hit| {
            let id = hit.get("_id").and_then(|v| v.as_str())?.to_string();
            let stored = hit.pointer(&format!("/_source/{}", field.replace('.', "/")))?.clone();
            Some((id, stored))
        })
        .collect()
}

/// The stored queries under `field` that any of the documents matches.
fn percolated(
    store: &Store,
    targets: &[String],
    field: &str,
    documents: &[Value],
) -> Vec<PercolateMatch> {
    if documents.is_empty() {
        return Vec::new();
    }
    let Some(scratch) = percolate_scratch(store, targets, field, documents) else {
        return Vec::new();
    };
    let mut matched = Vec::new();
    for (id, stored) in stored_queries(store, targets, field) {
        let asked = json!({"query": stored, "size": documents.len(), "_source": false});
        if let Ok(out) = run(&scratch, "_percolate", &asked, &Params::new())
            && out.total > 0
        {
            let score = out.max_score.unwrap_or(0.0);
            matched.push(PercolateMatch { id, score });
        }
    }
    matched
}

/// `_percolator_document_slot` and the highlights, on the hits a
/// `percolate` clause found.
///
/// The clause was rewritten into the ids of the rules it matched before the
/// search, so what each rule found is worked out again here for the page
/// alone: the slots of the documents it matched, and -- when the request
/// asks for highlighting -- each of those documents highlighted by the
/// rule's own query, under the field's name for a single document and under
/// `<slot>_<field>` for several, as the reference names them.
pub(crate) fn attach_percolate_slots(
    store: &Store,
    targets: &[String],
    body: &Value,
    page: &mut [Value],
) {
    let mut specs = Vec::new();
    if let Some(q) = body.get("query") {
        collect_percolates(q, &mut specs);
    }
    let named_several = specs.len() > 1;
    for spec in specs {
        let field = spec.get("field").and_then(|v| v.as_str()).unwrap_or("query").to_string();
        let Ok(documents) = percolate_documents(store, &spec) else { continue };
        if documents.is_empty() {
            continue;
        }
        let several = spec.get("documents").is_some();
        let Some(scratch) = percolate_scratch(store, targets, &field, &documents) else {
            continue;
        };
        let stored: std::collections::HashMap<String, Value> =
            stored_queries(store, targets, &field).into_iter().collect();
        let slot_field = match spec.get("name").and_then(|v| v.as_str()) {
            Some(name) if named_several || spec.get("name").is_some() => {
                format!("_percolator_document_slot_{name}")
            }
            _ => "_percolator_document_slot".to_string(),
        };
        for hit in page.iter_mut() {
            let Some(id) = hit.get("_id").and_then(|v| v.as_str()).map(str::to_string) else {
                continue;
            };
            let Some(query) = stored.get(&id) else { continue };
            let mut asked = json!({"query": query, "size": documents.len(), "_source": false});
            if let Some(h) = body.get("highlight") {
                asked["highlight"] = h.clone();
            }
            let Ok(out) = run(&scratch, "_percolate", &asked, &Params::new()) else { continue };
            let mut slots: Vec<(usize, Option<Value>)> = out
                .hits
                .iter()
                .filter_map(|h| {
                    let slot = h.get("_id").and_then(|v| v.as_str())?.parse().ok()?;
                    Some((slot, h.get("highlight").cloned()))
                })
                .collect();
            if slots.is_empty() {
                continue;
            }
            slots.sort_by_key(|(slot, _)| *slot);
            if !hit.get("fields").is_some_and(|f| f.is_object()) {
                hit["fields"] = json!({});
            }
            hit["fields"][&slot_field] = json!(slots.iter().map(|(s, _)| *s).collect::<Vec<_>>());
            for (slot, highlight) in slots {
                let Some(Value::Object(fields)) = highlight else { continue };
                if !hit.get("highlight").is_some_and(|f| f.is_object()) {
                    hit["highlight"] = json!({});
                }
                for (name, fragments) in fields {
                    let key = if several { format!("{slot}_{name}") } else { name };
                    hit["highlight"][key] = fragments;
                }
            }
        }
    }
}

fn collect_percolates(node: &Value, out: &mut Vec<Value>) {
    match node {
        Value::Object(o) => {
            if let Some(spec) = o.get("percolate") {
                out.push(spec.clone());
            }
            o.values().for_each(|v| collect_percolates(v, out));
        }
        Value::Array(items) => items.iter().for_each(|v| collect_percolates(v, out)),
        _ => {}
    }
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
            // a leaf query names its field as the key of its body, and a
            // stored query on a field nobody mapped is refused as the
            // reference refuses it, whatever the query's kind -- only
            // `query_string` was looked into, and a `term` on a misspelt
            // field was stored and never matched anything
            const LEAVES: &[&str] = &[
                "term",
                "terms",
                "match",
                "match_phrase",
                "match_phrase_prefix",
                "match_bool_prefix",
                "prefix",
                "wildcard",
                "regexp",
                "fuzzy",
                "range",
            ];
            let known = |named: &str| {
                named.contains('*')
                    || named.starts_with('_')
                    || g.mapping.type_of(named).is_some()
                    || g.mapping.types.keys().any(|k| k.starts_with(&format!("{named}.")))
            };
            for leaf in LEAVES {
                if let Some(Value::Object(body)) = o.get(*leaf) {
                    for key in body.keys() {
                        if matches!(key.as_str(), "boost" | "_name") {
                            continue;
                        }
                        if !known(key) {
                            return Some(key.clone());
                        }
                    }
                }
            }
            if let Some(named) =
                o.get("exists").and_then(|e| e.get("field")).and_then(|f| f.as_str())
                && !known(named)
            {
                return Some(named.to_string());
            }
            o.values().find_map(|v| unmapped_in_query(g, v))
        }
        Value::Array(items) => items.iter().find_map(|v| unmapped_in_query(g, v)),
        _ => None,
    }
}

#[cfg(test)]
mod more_like_this_tests {
    use super::*;

    #[test]
    fn minimum_should_match_counts_as_the_reference_counts() {
        assert_eq!(min_should_match(3, "30%"), 0);
        assert_eq!(min_should_match(10, "30%"), 3);
        assert_eq!(min_should_match(25, "30%"), 7);
        assert_eq!(min_should_match(5, "-1"), 4);
        assert_eq!(min_should_match(4, "2<50%"), 2);
    }

    #[test]
    fn words_come_back_in_java_hash_map_order() {
        let mut counts = JavaCounts::default();
        for word in ["party", "the", "agreement", "of"] {
            counts.add(word, 1);
        }
        counts.add("the", 2);
        let order: Vec<(String, u32)> = counts.in_java_order();
        // bucket = (h ^ h >>> 16) & 15 over Java's String.hashCode
        let bucket = |w: &str| {
            let mut h: i32 = 0;
            for u in w.encode_utf16() {
                h = h.wrapping_mul(31).wrapping_add(u as i32);
            }
            ((h ^ ((h as u32) >> 16) as i32) as u32 & 15) as usize
        };
        assert!(order.windows(2).all(|w| bucket(&w[0].0) <= bucket(&w[1].0)));
        assert_eq!(order.iter().find(|(w, _)| w == "the").map(|(_, n)| *n), Some(3));
    }
}
