//! What one caller may see of one index: the documents their DLS lets
//! through, the fields their FLS leaves in, and the values their masking
//! turns into hashes.
//!
//! A `View` is computed once per index per request on the request's own
//! task, then handed to whatever reads documents, so the rayon fan-out
//! never has to ask who is calling.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::IndexRestrictions;

/// The field name a hidden field is read as: nothing is mapped under it,
/// so a query on it matches nothing, an aggregation over it is empty and a
/// sort by it has no values.
pub const HIDDEN: &str = "__fls_hidden__";

pub struct View {
    pub index: String,
    pub dls: Option<Value>,
    restr: IndexRestrictions,
    salt: Vec<u8>,
}

/// The caller's views of a set of indices; empty while security is off.
pub type Views = HashMap<String, Arc<View>>;

/// The views of every target, or nothing at all while security is off.
pub fn views_for(store: &crate::store::Store, targets: &[String]) -> Views {
    let mut out = Views::new();
    if !store.security.enabled {
        return out;
    }
    let Some(caller) = super::layer::current_caller() else { return out };
    if caller.unrestricted {
        return out;
    }
    let cfg = store.security.config.read();
    for t in targets {
        let restr = cfg.restrictions(&caller, t);
        if !restr.reached {
            continue;
        }
        let dls = restr.dls_query();
        let view =
            View { index: t.clone(), dls, restr, salt: store.security.salt.as_bytes().to_vec() };
        if view.restricts() {
            out.insert(t.clone(), Arc::new(view));
        }
    }
    out
}

/// The caller's view of one index, if it is narrowed at all.
pub fn view_for(store: &crate::store::Store, index: &str) -> Option<Arc<View>> {
    views_for(store, &[index.to_string()]).remove(index)
}

impl View {
    /// Whether this view changes anything a caller could see.
    pub fn restricts(&self) -> bool {
        self.dls.is_some()
            || (!self.restr.unrestricted_fields && !self.restr.fls.is_empty())
            || (!self.restr.unmasked && !self.restr.masked.is_empty())
    }

    pub fn hidden(&self, field: &str) -> bool {
        !self.restr.field_visible(field)
    }

    pub fn masked(&self, field: &str) -> bool {
        self.restr.field_masked(field)
    }

    /// Hidden or masked: a query on such a field matches nothing.
    pub fn unqueryable(&self, field: &str) -> bool {
        self.hidden(field) || self.masked(field)
    }

    fn has_field_rules(&self) -> bool {
        (!self.restr.unrestricted_fields && !self.restr.fls.is_empty())
            || (!self.restr.unmasked && !self.restr.masked.is_empty())
    }

    /// A value the way the caller sees it: BLAKE2b-256 with the salt, hex.
    pub fn mask_text(&self, text: &str) -> String {
        hex(&blake2b256_salted(text.as_bytes(), &self.salt))
    }

    pub fn mask(&self, v: &Value) -> Value {
        match v {
            Value::Null => Value::Null,
            Value::Array(a) => Value::Array(a.iter().map(|x| self.mask(x)).collect()),
            Value::String(s) => Value::String(self.mask_text(s)),
            Value::Object(_) => v.clone(),
            other => Value::String(self.mask_text(&other.to_string())),
        }
    }

    /// A document's source, narrowed: hidden paths removed, masked paths hashed.
    pub fn filter_source(&self, src: &mut Value) {
        if !self.has_field_rules() {
            return;
        }
        self.walk(src, "");
    }

    fn walk(&self, v: &mut Value, prefix: &str) {
        match v {
            Value::Object(o) => {
                let keys: Vec<String> = o.keys().cloned().collect();
                for k in keys {
                    let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    if self.hidden(&path) {
                        o.remove(&k);
                        continue;
                    }
                    if self.masked(&path) {
                        let masked = self.mask(&o[&k]);
                        o.insert(k, masked);
                        continue;
                    }
                    if let Some(child) = o.get_mut(&k) {
                        self.walk(child, &path);
                    }
                }
            }
            Value::Array(a) => {
                for item in a.iter_mut() {
                    self.walk(item, prefix);
                }
            }
            _ => {}
        }
    }

    /// `fields` / `docvalue_fields` of a hit: keyed by dotted name.
    pub fn filter_fields(&self, fields: &mut Map<String, Value>) {
        if !self.has_field_rules() {
            return;
        }
        let keys: Vec<String> = fields.keys().cloned().collect();
        for k in keys {
            if self.hidden(&k) {
                fields.remove(&k);
            } else if self.masked(&k) {
                let masked = self.mask(&fields[&k]);
                fields.insert(k, masked);
            }
        }
    }

    /// One search hit, narrowed: source, fields, highlight, inner hits.
    pub fn filter_hit(&self, hit: &mut Value) {
        if !self.has_field_rules() {
            return;
        }
        if let Some(src) = hit.get_mut("_source") {
            self.filter_source(src);
        }
        if let Some(Value::Object(f)) = hit.get_mut("fields") {
            self.filter_fields(f);
        }
        if let Some(Value::Object(h)) = hit.get_mut("highlight") {
            let keys: Vec<String> = h.keys().cloned().collect();
            for k in keys {
                if self.unqueryable(&k) {
                    h.remove(&k);
                }
            }
        }
        if let Some(Value::Object(inner)) = hit.get_mut("inner_hits") {
            for (_, v) in inner.iter_mut() {
                if let Some(Value::Array(hits)) = v.pointer_mut("/hits/hits") {
                    for h in hits.iter_mut() {
                        self.filter_hit(h);
                    }
                }
            }
        }
    }

    // ---- queries -----------------------------------------------------------------

    /// The query with every clause over a hidden or masked field matching
    /// nothing, and such fields dropped from field lists.
    pub fn rewrite_query(&self, q: &Value, types: &HashMap<String, String>) -> Value {
        if !self.has_field_rules() {
            return q.clone();
        }
        self.rw(q, types)
    }

    /// `field:value` inside a query string, for a field the caller may not
    /// query, is turned to a field that is not there.
    fn rewrite_query_text(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let start_ok = i == 0
                || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_' || chars[i - 1] == '.');
            if start_ok && (chars[i].is_alphanumeric() || chars[i] == '_') {
                let mut j = i;
                while j < chars.len()
                    && (chars[j].is_alphanumeric()
                        || chars[j] == '_'
                        || chars[j] == '.'
                        || chars[j] == '*')
                {
                    j += 1;
                }
                if j < chars.len()
                    && chars[j] == ':'
                    && !(j + 1 < chars.len() && chars[j + 1] == ':')
                {
                    let name: String = chars[i..j].iter().collect();
                    if self.unqueryable(&name) {
                        out.push_str(HIDDEN);
                    } else {
                        out.push_str(&name);
                    }
                    i = j;
                    continue;
                }
            }
            out.push(chars[i]);
            i += 1;
        }
        out
    }

    fn rw(&self, v: &Value, types: &HashMap<String, String>) -> Value {
        const LEAF: &[&str] = &[
            "term",
            "terms",
            "terms_set",
            "match",
            "match_phrase",
            "match_phrase_prefix",
            "match_bool_prefix",
            "prefix",
            "wildcard",
            "regexp",
            "fuzzy",
            "range",
            "span_term",
            "geo_bounding_box",
            "geo_distance",
            "geo_polygon",
            "geo_shape",
            "intervals",
            "common",
            "knn",
            "neural",
        ];
        const FIELD_KEY: &[&str] = &["exists", "distance_feature", "rank_feature"];
        const FIELD_LISTS: &[&str] =
            &["multi_match", "query_string", "simple_query_string", "combined_fields"];
        match v {
            Value::Object(o) if o.len() == 1 => {
                let (k, inner) = o.iter().next().unwrap();
                if LEAF.contains(&k.as_str()) {
                    if let Some(io) = inner.as_object() {
                        let field = io
                            .keys()
                            .find(|f| !["boost", "_name", "ignore_unmapped"].contains(&f.as_str()))
                            .cloned();
                        if let Some(f) = field {
                            if self.unqueryable(&f) {
                                return json!({"match_none": {}});
                            }
                        }
                    }
                    return v.clone();
                }
                if FIELD_KEY.contains(&k.as_str()) {
                    if let Some(f) = inner.get("field").and_then(|f| f.as_str()) {
                        if self.unqueryable(f) {
                            return json!({"match_none": {}});
                        }
                    }
                    return v.clone();
                }
                if FIELD_LISTS.contains(&k.as_str()) {
                    let mut spec = inner.clone();
                    let mut emptied = false;
                    if k == "query_string" || k == "simple_query_string" {
                        if let Some(text) =
                            spec.get("query").and_then(|q| q.as_str()).map(|t| t.to_string())
                        {
                            if k == "query_string" {
                                spec["query"] = json!(self.rewrite_query_text(&text));
                            }
                        }
                        // with no field named, the whole mapping is searched:
                        // here, the part of it the caller may see
                        if spec.get("fields").is_none() && spec.get("default_field").is_none() {
                            let mut visible: Vec<String> = types
                                .iter()
                                .filter(|(_, kind)| !matches!(kind.as_str(), "object" | "nested"))
                                .map(|(n, _)| n.clone())
                                .filter(|n| !self.unqueryable(n))
                                .collect();
                            visible.sort();
                            if visible.is_empty() {
                                return json!({"match_none": {}});
                            }
                            spec["fields"] = json!(visible);
                        }
                    }
                    if let Some(Value::Array(fields)) = spec.get_mut("fields") {
                        let before = fields.len();
                        fields.retain(|f| {
                            f.as_str()
                                .map(|s| !self.unqueryable(s.split('^').next().unwrap_or(s)))
                                .unwrap_or(true)
                        });
                        emptied = before > 0 && fields.is_empty();
                    }
                    if let Some(df) = spec.get("default_field").and_then(|d| d.as_str()) {
                        if self.unqueryable(df) {
                            emptied = true;
                        }
                    }
                    if emptied {
                        return json!({"match_none": {}});
                    }
                    let mut out = Map::new();
                    out.insert(k.clone(), spec);
                    return Value::Object(out);
                }
                let mut out = Map::new();
                out.insert(k.clone(), self.rw(inner, types));
                Value::Object(out)
            }
            Value::Object(o) => {
                Value::Object(o.iter().map(|(k, x)| (k.clone(), self.rw(x, types))).collect())
            }
            Value::Array(a) => Value::Array(a.iter().map(|x| self.rw(x, types)).collect()),
            other => other.clone(),
        }
    }

    // ---- aggregations ---------------------------------------------------------------

    /// The aggregations with hidden fields read as unmapped, and masked
    /// bucket keys asked for in full so they can be hashed and cut after.
    pub fn rewrite_aggs(&self, aggs: &Value, types: &HashMap<String, String>) -> Value {
        if !self.has_field_rules() {
            return aggs.clone();
        }
        const METRIC: &[&str] = &[
            "max",
            "min",
            "avg",
            "sum",
            "stats",
            "extended_stats",
            "percentiles",
            "percentile_ranks",
            "median_absolute_deviation",
            "histogram",
            "date_histogram",
            "range",
            "date_range",
            "auto_date_histogram",
            "variable_width_histogram",
            "geo_bounds",
            "geo_centroid",
        ];
        let Some(o) = aggs.as_object() else { return aggs.clone() };
        let mut out = Map::new();
        for (name, spec) in o {
            let Some(so) = spec.as_object() else {
                out.insert(name.clone(), spec.clone());
                continue;
            };
            let mut new_spec = Map::new();
            for (ty, body) in so {
                if ty == "aggs" || ty == "aggregations" {
                    new_spec.insert(ty.clone(), self.rewrite_aggs(body, types));
                    continue;
                }
                if ty == "meta" {
                    new_spec.insert(ty.clone(), body.clone());
                    continue;
                }
                let mut b = body.clone();
                if let Some(f) = b.get("field").and_then(|f| f.as_str()).map(|s| s.to_string()) {
                    // a metric over a hidden field of a kind it cannot read
                    // fails as it would with the field in view
                    let kind = types.get(&f).map(|k| k.as_str()).unwrap_or("");
                    let wrong_kind = METRIC.contains(&ty.as_str())
                        && matches!(kind, "keyword" | "text" | "boolean" | "ip" | "binary");
                    if self.hidden(&f) && !wrong_kind {
                        b["field"] = json!(HIDDEN);
                    } else if self.masked(&f)
                        && matches!(ty.as_str(), "terms" | "significant_terms" | "rare_terms")
                    {
                        b["size"] = json!(65_536);
                        if let Some(bo) = b.as_object_mut() {
                            bo.remove("shard_size");
                        }
                    }
                }
                // a hidden field named inside a script-less top_hits keeps
                // its own filtering when the hits are written
                new_spec.insert(ty.clone(), b);
            }
            out.insert(name.clone(), Value::Object(new_spec));
        }
        Value::Object(out)
    }

    /// The aggregation results as the caller sees them: masked keys hashed
    /// and re-ordered, hits inside narrowed.
    pub fn post_aggs(&self, req: &Value, resp: &mut Value) {
        if !self.has_field_rules() {
            return;
        }
        let Some(ro) = req.as_object() else { return };
        for (name, spec) in ro {
            let Some(so) = spec.as_object() else { continue };
            let sub = so.get("aggs").or_else(|| so.get("aggregations"));
            let Some((ty, body)) =
                so.iter().find(|(k, _)| !["aggs", "aggregations", "meta"].contains(&k.as_str()))
            else {
                continue;
            };
            // typed keys may prefix the name
            let key = resp.as_object().and_then(|o| {
                o.keys().find(|k| *k == name || k.ends_with(&format!("#{name}"))).cloned()
            });
            let Some(key) = key else { continue };
            let Some(node) = resp.get_mut(&key) else { continue };
            let field = body.get("field").and_then(|f| f.as_str()).unwrap_or("");
            if ty == "terms" && self.masked(field) {
                self.mask_terms(body, node);
            }
            // top_hits come out of a search of their own, already narrowed
            if let Some(sub) = sub {
                match node.get_mut("buckets") {
                    Some(Value::Array(buckets)) => {
                        for b in buckets.iter_mut() {
                            self.post_aggs(sub, b);
                        }
                    }
                    Some(Value::Object(buckets)) => {
                        for (_, b) in buckets.iter_mut() {
                            self.post_aggs(sub, b);
                        }
                    }
                    _ => {
                        // a single-bucket aggregation carries its sub-aggs beside doc_count
                        self.post_aggs(sub, node);
                    }
                }
            }
        }
    }

    fn mask_terms(&self, body: &Value, node: &mut Value) {
        let Some(Value::Array(buckets)) = node.get_mut("buckets") else { return };
        for b in buckets.iter_mut() {
            if let Some(k) = b.get("key").cloned() {
                b["key"] = self.mask(&k);
                if let Some(bo) = b.as_object_mut() {
                    bo.remove("key_as_string");
                }
            }
        }
        let size = body.get("size").and_then(|s| s.as_u64()).unwrap_or(10) as usize;
        let (by_key, asc) = match body.get("order") {
            Some(Value::Object(o)) => {
                let (k, dir) = o
                    .iter()
                    .next()
                    .map(|(k, d)| (k.clone(), d.as_str().unwrap_or("asc") == "asc"))
                    .unwrap_or(("_count".into(), false));
                (k == "_key", dir)
            }
            Some(Value::Array(a)) => a
                .first()
                .and_then(|o| o.as_object())
                .and_then(|o| {
                    o.iter()
                        .next()
                        .map(|(k, d)| (k == "_key", d.as_str().unwrap_or("asc") == "asc"))
                })
                .unwrap_or((false, false)),
            _ => (false, false),
        };
        let key_of = |b: &Value| b.get("key").and_then(|k| k.as_str()).unwrap_or("").to_string();
        let count_of = |b: &Value| b.get("doc_count").and_then(|c| c.as_u64()).unwrap_or(0);
        if by_key {
            buckets.sort_by(|a, b| {
                if asc { key_of(a).cmp(&key_of(b)) } else { key_of(b).cmp(&key_of(a)) }
            });
        } else {
            buckets.sort_by(|a, b| {
                let c =
                    if asc { count_of(a).cmp(&count_of(b)) } else { count_of(b).cmp(&count_of(a)) };
                c.then_with(|| key_of(a).cmp(&key_of(b)))
            });
        }
        let total: u64 = buckets.iter().map(|b| count_of(b)).sum();
        buckets.truncate(size);
        let kept: u64 = buckets.iter().map(|b| count_of(b)).sum();
        let other = node.get("sum_other_doc_count").and_then(|v| v.as_u64()).unwrap_or(0);
        node["sum_other_doc_count"] = json!(other + total - kept);
        if node.get("doc_count_error_upper_bound").is_none() {
            node["doc_count_error_upper_bound"] = json!(0);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ---- BLAKE2b-256 with a salt, as the plugin masks -------------------------------

const IV: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];

const SIGMA: [[usize; 16]; 12] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
];

fn compress(h: &mut [u64; 8], block: &[u8; 128], t: u128, last: bool) {
    let mut m = [0u64; 16];
    for (i, w) in m.iter_mut().enumerate() {
        *w = u64::from_le_bytes(block[i * 8..i * 8 + 8].try_into().unwrap());
    }
    let mut v = [0u64; 16];
    v[..8].copy_from_slice(h);
    v[8..].copy_from_slice(&IV);
    v[12] ^= t as u64;
    v[13] ^= (t >> 64) as u64;
    if last {
        v[14] = !v[14];
    }
    #[inline(always)]
    fn g(v: &mut [u64; 16], a: usize, b: usize, c: usize, d: usize, x: u64, y: u64) {
        v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
        v[d] = (v[d] ^ v[a]).rotate_right(32);
        v[c] = v[c].wrapping_add(v[d]);
        v[b] = (v[b] ^ v[c]).rotate_right(24);
        v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
        v[d] = (v[d] ^ v[a]).rotate_right(16);
        v[c] = v[c].wrapping_add(v[d]);
        v[b] = (v[b] ^ v[c]).rotate_right(63);
    }
    for s in &SIGMA {
        g(&mut v, 0, 4, 8, 12, m[s[0]], m[s[1]]);
        g(&mut v, 1, 5, 9, 13, m[s[2]], m[s[3]]);
        g(&mut v, 2, 6, 10, 14, m[s[4]], m[s[5]]);
        g(&mut v, 3, 7, 11, 15, m[s[6]], m[s[7]]);
        g(&mut v, 0, 5, 10, 15, m[s[8]], m[s[9]]);
        g(&mut v, 1, 6, 11, 12, m[s[10]], m[s[11]]);
        g(&mut v, 2, 7, 8, 13, m[s[12]], m[s[13]]);
        g(&mut v, 3, 4, 9, 14, m[s[14]], m[s[15]]);
    }
    for i in 0..8 {
        h[i] ^= v[i] ^ v[i + 8];
    }
}

/// BLAKE2b with a 32-byte digest and a salt of up to 16 bytes, no key.
pub fn blake2b256_salted(data: &[u8], salt: &[u8]) -> [u8; 32] {
    let mut h = IV;
    // parameter block: digest length, key length 0, fanout 1, depth 1
    h[0] ^= 0x0101_0000 | 32;
    let mut s = [0u8; 16];
    let n = salt.len().min(16);
    s[..n].copy_from_slice(&salt[..n]);
    h[4] ^= u64::from_le_bytes(s[..8].try_into().unwrap());
    h[5] ^= u64::from_le_bytes(s[8..].try_into().unwrap());
    let mut t: u128 = 0;
    let mut chunks = data.chunks(128).peekable();
    if data.is_empty() {
        compress(&mut h, &[0u8; 128], 0, true);
    }
    while let Some(chunk) = chunks.next() {
        let mut block = [0u8; 128];
        block[..chunk.len()].copy_from_slice(chunk);
        t += chunk.len() as u128;
        let last = chunks.peek().is_none();
        compress(&mut h, &block, t, last);
    }
    let mut out = [0u8; 32];
    for i in 0..4 {
        out[i * 8..i * 8 + 8].copy_from_slice(&h[i].to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn masks_like_the_plugin() {
        let d = super::blake2b256_salted(b"000-00-0001", b"e1ukloTsQlOgPquJ");
        assert_eq!(
            super::hex(&d),
            "71f04b780e30feb07885afd52f403c273d9ba976f0b1c3c4226c5be1e45bc797"
        );
    }
}
