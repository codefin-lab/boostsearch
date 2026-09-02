//! Running a script on its own: `_scripts/painless/_execute`, and the list of
//! places a script may run.

use super::*;
use crate::painless::contexts::{Compiled, Runner};

/// Every context a script may be written for, as OpenSearch lists them.
const CONTEXTS: &[&str] = &[
    "aggregation_selector",
    "aggs",
    "aggs_combine",
    "aggs_init",
    "aggs_map",
    "aggs_reduce",
    "analysis",
    "bucket_aggregation",
    "context_aware_grouping",
    "derived_field",
    "field",
    "filter",
    "ingest",
    "interval",
    "moving-function",
    "number_sort",
    "painless_test",
    "processor_conditional",
    "score",
    "script_heuristic",
    "search",
    "similarity",
    "similarity_weight",
    "string_sort",
    "template",
    "terms_set",
    "update",
];

/// What each context lets a script reach -- the classes, methods and
/// bindings -- as OpenSearch answers `_context?context=…`, carried
/// compressed and unpacked when first asked for.
macro_rules! whitelists {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_bytes!(concat!("../painless/whitelist/", $name, ".json.gz")))),*]
    };
}
const WHITELISTS: &[(&str, &[u8])] = whitelists![
    "aggregation_selector",
    "aggs",
    "aggs_combine",
    "aggs_init",
    "aggs_map",
    "aggs_reduce",
    "analysis",
    "bucket_aggregation",
    "context_aware_grouping",
    "derived_field",
    "field",
    "filter",
    "ingest",
    "interval",
    "moving-function",
    "number_sort",
    "painless_test",
    "processor_conditional",
    "score",
    "script_heuristic",
    "search",
    "similarity",
    "similarity_weight",
    "string_sort",
    "template",
    "terms_set",
    "update",
];

fn whitelist(name: &str) -> Option<Value> {
    use std::io::Read;
    let (_, packed) = WHITELISTS.iter().find(|(n, _)| *n == name)?;
    let mut text = String::new();
    flate2::read::GzDecoder::new(*packed).read_to_string(&mut text).ok()?;
    serde_json::from_str(&text).ok()
}

/// A script that would not compile, reported the way a stored script's
/// compilation reports it: the script exception stands at the top.
pub(crate) fn compile_failure(e: crate::painless::ScriptError) -> Response {
    let detail = e.to_json();
    let mut root = detail.clone();
    if let Some(o) = root.as_object_mut() {
        o.remove("caused_by");
    }
    let mut top = detail;
    if let Some(o) = top.as_object_mut() {
        o.insert("root_cause".into(), json!([root]));
    }
    let body = json!({"error": top, "status": 400});
    (StatusCode::BAD_REQUEST, axum::Json(body)).into_response()
}

pub async fn painless_execute(
    State(store): State<Store>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    let Some(spec) = body.get("script") else {
        return err(StatusCode::BAD_REQUEST, "illegal_argument_exception", "[script] is required");
    };
    let compiled = match Compiled::of(spec, &|named| store.stored_script(named)) {
        Ok(c) => c,
        Err(e) => return crate::api::script_failure(e),
    };
    let context = body.get("context").and_then(|v| v.as_str()).unwrap_or("painless_test");
    let setup = body.get("context_setup").cloned().unwrap_or(json!({}));
    let mut runner = Runner::new(&compiled.params);
    // a document is written into the named index's shape, without being
    // written into the index
    if let Some(document) = setup.get("document") {
        let index = setup.get("index").and_then(|v| v.as_str()).unwrap_or_default();
        let mapping = store.get(index).map(|st| st.read().mapping.clone()).unwrap_or_default();
        let indexed = crate::store::expand_for_indexing(document.clone(), &mapping);
        runner = runner.with_doc(&indexed, &mapping).with_score(1.0);
    }
    let out = match runner.run(&compiled.script) {
        Ok(v) => v,
        Err(e) => return crate::api::script_failure(e),
    };
    let result = match context {
        // a test answers with the value written out; a filter with whether
        // it held; a score with a number
        "painless_test" => json!(out.as_text()),
        "filter" => json!(matches!(out, crate::painless::Value::Bool(true))),
        "score" => json!(out.as_f64().unwrap_or(0.0)),
        _ => out.to_json(),
    };
    respond(&p, json!({"result": result}))
}

pub async fn painless_contexts(Query(p): Query<Params>) -> Response {
    match p.get("context") {
        None => respond(&p, json!({"contexts": CONTEXTS})),
        Some(named) => match whitelist(named) {
            Some(found) => respond(&p, found),
            None => err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!("script context [{named}] not found"),
            ),
        },
    }
}
