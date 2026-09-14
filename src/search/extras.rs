//! What a query needs beyond the query: the clauses that are settled from a
//! document's own values once the candidates are known.

use super::*;

/// Does this cluster still allow the queries that cost the most to run?
pub(crate) fn expensive_allowed(store: &Store) -> bool {
    store
        .cluster_setting("search.allow_expensive_queries")
        .map(|v| v != json!("false") && v != json!(false))
        .unwrap_or(true)
}

pub(crate) fn scan_extras(node: &Value, out: &mut Extras) {
    match node {
        Value::Object(o) => {
            for (k, v) in o {
                match k.as_str() {
                    "geo_shape" | "geo_bounding_box" | "geo_distance" | "geo_polygon" => {
                        out.geo = true
                    }
                    "distance_feature" => out.distance_feature = true,
                    "_name" => out.named = true,
                    "nested" => {
                        out.nested_query = true;
                        if v.get("inner_hits").is_some() {
                            out.nested_inner_hits = true;
                        }
                    }
                    _ => {}
                }
                scan_extras(v, out);
            }
        }
        Value::Array(a) => {
            for v in a {
                scan_extras(v, out);
            }
        }
        _ => {}
    }
}

/// The `distance_feature` clause of a query, wherever it sits.
pub(crate) fn find_distance_feature(node: &Value) -> Option<&Value> {
    match node {
        Value::Object(o) => {
            if let Some(spec) = o.get("distance_feature") {
                return Some(spec);
            }
            o.values().find_map(find_distance_feature)
        }
        Value::Array(a) => a.iter().find_map(find_distance_feature),
        _ => None,
    }
}

/// How far apart two moments are, in whatever unit the values are counted in.
pub(crate) fn date_distance(origin: &Value, value: &Value) -> Option<f64> {
    let read = |v: &Value| -> Option<f64> {
        match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => crate::store::canonical_date(&json!(s))
                .and_then(|d| crate::store::parse_date_lenient(&d))
                .map(|d| d.unix_timestamp_nanos() as f64),
            _ => None,
        }
    };
    Some((read(origin)? - read(value)?).abs())
}

/// A length of time, written the way a pivot is: a count and a unit.
pub(crate) fn parse_time_amount(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    let (n, unit) = s.split_at(split);
    let n: f64 = n.parse().ok()?;
    Some(
        n * match unit {
            "nanos" => 1.0,
            "micros" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" | "H" => 3_600e9,
            "d" => 86_400e9,
            _ => return None,
        },
    )
}

/// Whether every geo clause in a query sits where narrowing the whole answer
/// by it means the same thing.
///
/// A geo clause is not built as a query: the query says only that the field
/// is there, and the real predicate is applied to the candidates afterwards,
/// as a narrowing of the whole answer. (`intervals` was answered the same way
/// and is now a query of its own, which may stand anywhere a query can.) That is only the same thing when the
/// clause is AND-ed with everything else from the root -- inside `should` it
/// dropped documents that matched a sibling, and inside `must_not` it dropped
/// every document instead of the ones inside the shape. Only the first clause
/// of each kind was applied, too, so a second was ignored outright. Where the
/// shape does not hold, the request is refused rather than answered wrongly.
pub(crate) fn placement_complaint(query: &Value) -> Option<String> {
    fn walk(node: &Value, conjunctive: bool, geo: &mut usize) -> bool {
        let Some(o) = node.as_object() else {
            return match node {
                Value::Array(a) => a.iter().all(|v| walk(v, conjunctive, geo)),
                _ => true,
            };
        };
        if crate::search::geo::is_geo_clause(o) {
            *geo += 1;
            return conjunctive;
        }
        if let Some(inner) = o.get("bool").and_then(|b| b.as_object()) {
            return inner.iter().all(|(k, v)| {
                let still = conjunctive && matches!(k.as_str(), "must" | "filter");
                walk(v, still, geo)
            });
        }
        if let Some(inner) = o.get("constant_score").and_then(|c| c.get("filter")) {
            return walk(inner, conjunctive, geo);
        }
        // A `function_score` matches exactly what its inner query matches --
        // the functions move scores and never the set -- so a clause under it
        // still narrows the whole answer. The same holds for the `positive`
        // side of a `boosting`, whose `negative` only demotes. Refusing these
        // turned away a query the reference answers, and the commonest shape
        // of all: filter by distance, then rank by a decay over it.
        if let Some(fs) = o.get("function_score").and_then(|f| f.as_object()) {
            return fs.iter().all(|(k, v)| {
                let still = conjunctive && matches!(k.as_str(), "query" | "filter");
                walk(v, still, geo)
            });
        }
        if let Some(bs) = o.get("boosting").and_then(|b| b.as_object()) {
            return bs.iter().all(|(k, v)| {
                let still = conjunctive && k == "positive";
                walk(v, still, geo)
            });
        }
        // anywhere else -- a nested query, a function score, a should -- the
        // clause is no longer a narrowing of the whole answer
        o.values().all(|v| walk(v, false, geo))
    }
    let mut geo = 0usize;
    let placed = walk(query, true, &mut geo);
    if !placed {
        return Some(
            "a geo clause is answered by narrowing the whole result, so it may \
             only stand where it narrows the whole result: at the top of the query, or \
             under `must` or `filter`"
                .to_string(),
        );
    }
    if geo > 1 {
        return Some("only one geo clause can be answered in a query".to_string());
    }
    None
}

/// The `nested` clauses of a query that can be settled by reading the
/// candidates' own objects.
///
/// A document is stored whole here rather than split into a parent and its
/// children, so the query built for a `nested` clause drops the path and asks
/// the inner query of the document: a document where one object of the array
/// answers one clause and a *different* object answers the other matches,
/// where the reference requires one object to answer both. That is the
/// difference the sixth review wrote down and did not close, saying it needed
/// each object indexed as a document of its own.
///
/// It does not. The objects are in the source, `object_matches` already reads
/// one of them for a sort filter, for `inner_hits` and for every nested
/// aggregation -- the same answer the query needs, computed and thrown away.
/// A document whose `inner_hits` are empty was being returned as a match by
/// the very engine that had just failed to name a matching object.
///
/// So the clause is settled like a geo shape: the query finds the candidates,
/// and each candidate keeps its place only if one of its objects answers the
/// whole inner query. Two conditions, both conservative:
///
///   * the clause must narrow the whole answer, as geo must -- inside
///     `should` or `must_not` dropping a candidate means something else;
///   * the inner query must be written in clauses `object_matches` answers
///     exactly. It answers anything else with `true`, which is safe for a
///     filter that only ever narrows, and would be a wrong answer here: a
///     `match` is judged on whole lowercased words rather than on the field's
///     own analyzer, and could drop a document that really does match.
///
/// Where either fails the clause is left as it was, which is the old answer
/// rather than a new wrong one.
pub(crate) fn settleable_nested(query: &Value) -> Vec<(String, Value)> {
    fn exactly_answered(node: &Value) -> bool {
        let Some(o) = node.as_object() else { return false };
        let Some((kind, body)) = o.iter().next() else { return false };
        match kind.as_str() {
            "match_all" | "term" | "range" | "exists" => true,
            "bool" => body.as_object().is_some_and(|b| {
                b.iter().all(|(k, v)| match k.as_str() {
                    "must" | "filter" | "must_not" => match v {
                        Value::Array(a) => a.iter().all(exactly_answered),
                        one => exactly_answered(one),
                    },
                    // a `should` is a floor on how many clauses answer, which
                    // `object_matches` does not count
                    _ => false,
                })
            }),
            _ => false,
        }
    }
    fn walk(node: &Value, conjunctive: bool, out: &mut Vec<(String, Value)>) {
        match node {
            Value::Object(o) => {
                if let Some(spec) = o.get("nested").and_then(|n| n.as_object()) {
                    let path = spec.get("path").and_then(|p| p.as_str()).unwrap_or("");
                    if let Some(inner) = spec.get("query")
                        && conjunctive
                        && !path.is_empty()
                        && exactly_answered(inner)
                    {
                        out.push((path.to_string(), inner.clone()));
                    }
                    // inside a nested clause the objects are the documents, so
                    // a clause deeper in is a different question
                    return;
                }
                if let Some(inner) = o.get("bool").and_then(|b| b.as_object()) {
                    // a bool with nothing but one `should` clause needs that
                    // clause: `minimum_should_match` is one when there is no
                    // `must` or `filter`, so the single clause is required
                    let lone_should = inner.len() == 1
                        && inner
                            .get("should")
                            .map(|v| match v {
                                Value::Array(a) => a.len() == 1,
                                _ => true,
                            })
                            .unwrap_or(false);
                    for (k, v) in inner {
                        let still = conjunctive
                            && (matches!(k.as_str(), "must" | "filter")
                                || (lone_should && k == "should"));
                        walk(v, still, out);
                    }
                    return;
                }
                if let Some(inner) = o.get("constant_score").and_then(|c| c.get("filter")) {
                    walk(inner, conjunctive, out);
                    return;
                }
                if let Some(fs) = o.get("function_score").and_then(|f| f.as_object()) {
                    for (k, v) in fs {
                        walk(v, conjunctive && matches!(k.as_str(), "query" | "filter"), out);
                    }
                    return;
                }
                for v in o.values() {
                    walk(v, false, out);
                }
            }
            Value::Array(a) => {
                for v in a {
                    walk(v, conjunctive, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(query, true, &mut out);
    out
}

/// Every field an inner `nested` query names.
pub(crate) fn fields_named(node: &Value) -> Vec<String> {
    fn walk(node: &Value, out: &mut Vec<String>) {
        let Some(o) = node.as_object() else {
            if let Value::Array(a) = node {
                for v in a {
                    walk(v, out);
                }
            }
            return;
        };
        for (kind, body) in o {
            match kind.as_str() {
                "term" | "range" => {
                    if let Some((f, _)) = body.as_object().and_then(|b| b.iter().next()) {
                        out.push(f.clone());
                    }
                }
                "exists" => {
                    if let Some(f) = body.get("field").and_then(|f| f.as_str()) {
                        out.push(f.to_string());
                    }
                }
                _ => walk(body, out),
            }
        }
    }
    let mut out = Vec::new();
    walk(node, &mut out);
    out
}

/// Whether the mapping says every field the clause names is a plain leaf this
/// can read out of one object.
///
/// A `flat_object` holds a whole tree under one name, and a term against the
/// field itself matches a value anywhere inside it -- `{"term": {"issue.labels":
/// "2023-01-01"}}` finds the document whose `labels.createdDate` is that date.
/// Reading `labels` out of the object and comparing the tree with the string
/// says no, and the post-filter dropped a document the query had rightly
/// found. The same goes for a field the mapping does not declare: there is no
/// knowing what matching it meant.
pub(crate) fn leaves_only(mapping: &crate::store::Mapping, path: &str, inner: &Value) -> bool {
    fields_named(inner).iter().all(|f| {
        let full =
            if f.starts_with(&format!("{path}.")) { f.clone() } else { format!("{path}.{f}") };
        match mapping.type_of(&full).or_else(|| mapping.type_of(f)) {
            Some("flat_object") | Some("object") | Some("nested") | None => false,
            Some(_) => true,
        }
    })
}

/// The query to find candidates with, for a body whose `nested` clauses are
/// settled afterwards.
///
/// The post-filter can only take candidates away. A `must_not` inside a
/// settleable `nested` clause, built as it stands, asks that *no object* of
/// the document answers it -- and drops documents the right answer keeps, so
/// there is nothing left for the post-filter to accept. Those clauses are
/// dropped from the query that finds the candidates and left to the
/// post-filter, which reads them against one object at a time.
pub(crate) fn relaxed_for_nested(query: &Value) -> Value {
    let settleable = settleable_nested(query);
    if settleable.is_empty() {
        return query.clone();
    }
    fn strip(node: &mut Value, paths: &[String]) {
        match node {
            Value::Object(o) => {
                if let Some(spec) = o.get_mut("nested").and_then(|n| n.as_object_mut()) {
                    let here = spec.get("path").and_then(|p| p.as_str()).unwrap_or("").to_string();
                    if paths.contains(&here)
                        && let Some(inner) = spec.get_mut("query").and_then(|q| q.as_object_mut())
                        && let Some(b) = inner.get_mut("bool").and_then(|b| b.as_object_mut())
                    {
                        b.remove("must_not");
                        // a bool left with nothing asks for everything
                        if b.is_empty() {
                            b.insert("must".into(), serde_json::json!([{"match_all": {}}]));
                        }
                    }
                    return;
                }
                for v in o.values_mut() {
                    strip(v, paths);
                }
            }
            Value::Array(a) => {
                for v in a {
                    strip(v, paths);
                }
            }
            _ => {}
        }
    }
    let paths: Vec<String> = settleable.into_iter().map(|(p, _)| p).collect();
    let mut out = query.clone();
    strip(&mut out, &paths);
    out
}

pub(crate) fn settle_by_value(
    cands: &mut Vec<Cand>,
    searchers: &Searchers,
    body: &Value,
    extras: &Extras,
) {
    // A `nested` clause is answered against one object of the array rather
    // than against the document's fields taken together.
    for (path, inner) in body.get("query").map(settleable_nested).unwrap_or_default() {
        cands.retain(|c| {
            let (_, searcher, st) = &searchers[c.shard];
            let g = st.read();
            let Some((_, src)) = source_of(searcher, &g, c.addr) else { return true };
            if !leaves_only(&g.mapping, &path, &inner) {
                return true;
            }
            let src = derived_copy(src, &g.mapping);
            let mut objects = Vec::new();
            crate::search::nested::gather_objects(&src, &path, "", &mut objects);
            objects.iter().any(|(_, o)| crate::search::nested::object_matches(&inner, o, &path))
        });
    }
    // A geo query asks where a point is. The query built for it only says the
    // field is there, so each candidate's own position is read and placed.
    if let Some((field, shape)) =
        extras.geo.then(|| body.get("query").and_then(find_geo_clause)).flatten()
    {
        let path = format!("/{}", field.replace('.', "/"));
        cands.retain(|c| {
            let (_, searcher, st) = &searchers[c.shard];
            let g = st.read();
            let Some((_, src)) = source_of(searcher, &g, c.addr) else { return true };
            let src = derived_copy(src, &g.mapping);
            let Some(here) = src.pointer(&path) else { return false };
            // a field may hold one point or several; a pair of numbers is one
            let points: Vec<&Value> = match here {
                Value::Array(a) if a.iter().all(|v| v.is_number()) => vec![here],
                Value::Array(a) => a.iter().collect(),
                other => vec![other],
            };
            points.iter().any(|p| point_within(&shape, p))
        });
    }
    // `distance_feature` scores by how near a value is to an origin. The
    // candidates are known by now, and each one's value can simply be read.
    if let Some(spec) =
        extras.distance_feature.then(|| body.get("query").and_then(find_distance_feature)).flatten()
    {
        let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
        let path = format!("/{}", field.replace('.', "/"));
        let pivot = spec.get("pivot").and_then(|v| v.as_str()).unwrap_or("");
        let origin = spec.get("origin").cloned().unwrap_or(Value::Null);
        // a point may be written as an object as well as a pair or text, and
        // a field mapped as a point is a point whatever the origin looks
        // like: an origin written `{lat, lon}` was read as a date, found no
        // distance, and left every score as the query gave it
        let written_geo = origin.is_array()
            || origin.get("lat").is_some()
            || origin.as_str().map(|s| s.contains(',')).unwrap_or(false);
        for c in cands.iter_mut() {
            let (_, searcher, st) = &searchers[c.shard];
            let g = st.read();
            let geo = written_geo || g.mapping.type_of(&field) == Some("geo_point");
            let Some((_, src)) = source_of(searcher, &g, c.addr) else { continue };
            let src = derived_copy(src, &g.mapping);
            let Some(value) = src.pointer(&path) else { continue };
            let distance = if geo {
                geo_distance_metres(&origin, value)
            } else {
                date_distance(&origin, value)
            };
            let pivot_size = if geo { parse_distance(pivot) } else { parse_time_amount(pivot) };
            match (distance, pivot_size) {
                (Some(d), Some(p)) if p > 0.0 => c.score = (p / (p + d)) as f32,
                _ => {}
            }
        }
    }
}

/// The source with its derived fields in, where the mapping has any.
fn derived_copy(src: Value, mapping: &crate::store::Mapping) -> Value {
    if mapping.derived_fields().is_empty() {
        src
    } else {
        crate::store::with_derived(&src, mapping)
    }
}
