//! `_source`: what of a document goes back, and in what shape.

use super::*;

/// Read a document's `_source`, honouring uncommitted writes so GET is realtime.
pub(crate) fn source_enabled(st: &IdxState) -> bool {
    st.mapping
        .raw
        .get("_source")
        .and_then(|s| s.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// The document as a search would see it: only what has been refreshed.
///
/// `realtime=false` asks for exactly that -- the reader's view rather than the
/// writer's -- which is how a caller checks whether a write is visible yet.
pub fn read_source_refreshed(st: &IdxState, id: &str) -> Option<Value> {
    let searcher = st.reader.searcher();
    let q = TermQuery::new(Term::from_field_text(st.fields.id, id), IndexRecordOption::Basic);
    let hits = searcher.search(&q, &TopDocs::with_limit(1).order_by_score()).ok()?;
    let (_, addr) = hits.first()?;
    let doc: TantivyDocument = searcher.doc(*addr).ok()?;
    let raw = doc.get_first(st.fields.source)?.as_str()?.to_string();
    serde_json::from_str(&raw).ok()
}

/// The document however it can be reached: `realtime` is the default, and
/// sees writes that have been flushed but not yet refreshed.
pub fn read_source_as_asked(st: &IdxState, id: &str, p: &Params) -> Option<Value> {
    if p.get("realtime").map(|v| v == "false").unwrap_or(false) {
        return read_source_refreshed(st, id);
    }
    read_source(st, id)
}

pub fn read_source(st: &IdxState, id: &str) -> Option<Value> {
    st.gets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Some(p) = st.pending.get(id) {
        return p.as_ref().and_then(|raw| serde_json::from_str(raw).ok());
    }
    // the realtime reader sees flushed-but-unrefreshed writes
    let searcher = st.realtime.searcher();
    let q = TermQuery::new(Term::from_field_text(st.fields.id, id), IndexRecordOption::Basic);
    let hits = searcher.search(&q, &TopDocs::with_limit(1).order_by_score()).ok()?;
    let (_, addr) = hits.first()?;
    let doc: TantivyDocument = searcher.doc(*addr).ok()?;
    let raw = doc.get_first(st.fields.source)?.as_str()?.to_string();
    serde_json::from_str(&raw).ok()
}

/// `refresh=true` on a read asks for the index to be brought up to date first,
/// so that a write made a moment ago is visible to it.
pub(crate) fn refresh_before_read(store: &Store, index: &str, p: &Params) {
    if !flag(p, "refresh") {
        return;
    }
    for n in store.resolve(index) {
        if let Some(st) = store.get(&n) {
            let _ = st.write().refresh();
        }
    }
}

pub async fn get_source(
    State(store): State<Store>,
    Path((index, id)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    refresh_before_read(&store, &index, &p);
    let Some(st) = store.get(&index) else {
        return if ignored(&p, StatusCode::NOT_FOUND) {
            (StatusCode::NOT_FOUND, axum::Json(json!({}))).into_response()
        } else {
            no_such_index(&index)
        };
    };
    let g = st.read();
    if !source_enabled(&g) {
        return err(
            StatusCode::NOT_FOUND,
            "illegal_argument_exception",
            format!("fields [_source] are disabled in the mappings for index [{}]", g.name),
        );
    }
    match read_source_as_asked(&g, &id, &p)
        .filter(|_| routing_matches(&g, &id, &p))
        .filter(|_| crate::security::doc_visible(&store, &g, &id))
    {
        Some(mut src) => {
            crate::security::audit_document_read(&g.name, &id, &src);
            crate::security::narrow_source(&store, &g.name, &mut src);
            axum::Json(filter_source_params(&src, &p)).into_response()
        }
        None => err(
            StatusCode::NOT_FOUND,
            "resource_not_found_exception",
            format!("Document not found [{}]/[{id}]", g.name),
        ),
    }
}

pub(crate) fn filter_source_params(src: &Value, p: &Params) -> Value {
    if let Some(v) = p.get("_source") {
        if v == "false" {
            return json!(null);
        }
        if v != "true" {
            return crate::source::filter(src, &as_list(p, "_source").unwrap_or_default(), &[]);
        }
    }
    let inc = as_list(p, "_source_includes").or_else(|| as_list(p, "_source_include"));
    let exc = as_list(p, "_source_excludes").or_else(|| as_list(p, "_source_exclude"));
    if inc.is_none() && exc.is_none() {
        return src.clone();
    }
    crate::source::filter(src, &inc.unwrap_or_default(), &exc.unwrap_or_default())
}

/// `_source` as it can appear in a request body: bool, pattern, list, or
/// an object with includes/excludes.
pub fn apply_source_selector(src: &Value, sel: &Value) -> Value {
    match sel {
        Value::Bool(true) | Value::Null => src.clone(),
        Value::Bool(false) => Value::Null,
        Value::String(pat) => crate::source::filter(src, std::slice::from_ref(pat), &[]),
        Value::Array(items) => {
            let inc: Vec<String> =
                items.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
            crate::source::filter(src, &inc, &[])
        }
        Value::Object(o) => {
            let pick = |k1: &str, k2: &str| -> Vec<String> {
                let v = o.get(k1).or_else(|| o.get(k2));
                match v {
                    Some(Value::String(s)) => vec![s.clone()],
                    Some(Value::Array(a)) => {
                        a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()
                    }
                    _ => vec![],
                }
            };
            crate::source::filter(src, &pick("includes", "include"), &pick("excludes", "exclude"))
        }
        _ => src.clone(),
    }
}

/// The `_source*` query-string family, as a selector value.
pub(crate) fn source_selector_from_params(p: &Params) -> Option<Value> {
    if let Some(v) = p.get("_source") {
        return Some(match v.as_str() {
            "true" => json!(true),
            "false" => json!(false),
            other if other.contains(',') => {
                json!(other.split(',').map(|s| s.trim()).collect::<Vec<_>>())
            }
            other => json!(other),
        });
    }
    let inc = as_list(p, "_source_includes").or_else(|| as_list(p, "_source_include"));
    let exc = as_list(p, "_source_excludes").or_else(|| as_list(p, "_source_exclude"));
    if inc.is_none() && exc.is_none() {
        return None;
    }
    Some(json!({
        "includes": inc.unwrap_or_default(),
        "excludes": exc.unwrap_or_default(),
    }))
}

/// `stored_fields` is answered from `_source`; every value comes back as a list.
pub(crate) fn wants_source_via_stored_fields(p: &Params) -> bool {
    p.get("stored_fields").map(|s| s.split(',').any(|f| f.trim() == "_source")).unwrap_or(false)
}

pub(crate) fn stored_fields(src: &Value, p: &Params) -> Option<Value> {
    let spec = p.get("stored_fields")?;
    if spec == "_none_" || spec.is_empty() {
        return None;
    }
    let mut out = serde_json::Map::new();
    for name in spec.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if name == "_source" {
            continue;
        }
        let picked = crate::source::filter(src, &[name.to_string()], &[]);
        if let Some(v) = flat_lookup(&picked, name) {
            let arr = match v {
                Value::Array(a) => Value::Array(a),
                other => Value::Array(vec![other]),
            };
            out.insert(name.to_string(), arr);
        }
    }
    if out.is_empty() { None } else { Some(Value::Object(out)) }
}

pub(crate) fn flat_lookup(v: &Value, path: &str) -> Option<Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur.clone())
}
