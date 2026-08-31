//! Pipelines, for documents on the way in and answers on the way out.

use super::*;

/// The keys a pipeline of each kind is allowed to carry. Anything else is a
/// mistake worth naming rather than storing.
pub(crate) fn pipeline_keys(kind: &str) -> &'static [&'static str] {
    match kind {
        "search" => &[
            "description",
            "version",
            "request_processors",
            "response_processors",
            "phase_results_processors",
            "_meta",
        ],
        _ => &["description", "version", "processors", "on_failure", "_meta"],
    }
}

pub(crate) async fn put_pipeline(
    store: Store,
    kind: &str,
    name: String,
    p: Params,
    body: String,
) -> Response {
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(o) = body.as_object() else {
        return err(
            StatusCode::BAD_REQUEST,
            "parse_exception",
            "[pipeline] pipeline definition is not an object",
        );
    };
    let allowed = pipeline_keys(kind);
    let stray = o.keys().map(|k| k.to_string()).find(|k| !allowed.contains(&k.as_str()));
    if let Some(stray) = stray {
        return err(
            StatusCode::BAD_REQUEST,
            "parse_exception",
            format!(
                "[{stray}] pipeline doesn't support one or more provided configuration \
                     parameters"
            ),
        );
    }
    store.put_pipeline(kind, &name, body);
    respond(&p, json!({"acknowledged": true}))
}

pub(crate) async fn get_pipeline(
    store: Store,
    kind: &str,
    name: Option<String>,
    p: Params,
) -> Response {
    let all = store.pipelines(kind);
    let want = name.filter(|n| !n.is_empty());
    let picked: serde_json::Map<String, Value> = all
        .into_iter()
        .filter(|(n, _)| match want.as_deref() {
            None => true,
            Some(pat) => pat.split(',').any(|w| {
                let w = w.trim();
                w == "*" || w == n || crate::store::glob_match(w, n)
            }),
        })
        .collect();
    // asking after one pipeline that is not there is a miss; asking after all
    // of them when there are none is simply an empty answer
    if picked.is_empty() && want.as_deref().map(|w| !w.contains('*')).unwrap_or(false) {
        return err(
            StatusCode::NOT_FOUND,
            "resource_not_found_exception",
            format!("pipeline [{}] is missing", want.unwrap_or_default()),
        );
    }
    respond(&p, Value::Object(picked))
}

pub(crate) async fn delete_pipeline(store: Store, kind: &str, name: String, p: Params) -> Response {
    if store.remove_pipelines(kind, &name) == 0 && !name.contains('*') {
        return err(
            StatusCode::NOT_FOUND,
            "resource_not_found_exception",
            format!("pipeline [{name}] is missing"),
        );
    }
    respond(&p, json!({"acknowledged": true}))
}

pub async fn put_ingest_pipeline(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    put_pipeline(store, "ingest", name, p, body).await
}

pub async fn get_ingest_pipeline(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    get_pipeline(store, "ingest", name.map(|Path(n)| n), p).await
}

pub async fn delete_ingest_pipeline(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    delete_pipeline(store, "ingest", name, p).await
}

pub async fn put_search_pipeline(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    put_pipeline(store, "search", name, p, body).await
}

pub async fn get_search_pipeline(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
) -> Response {
    get_pipeline(store, "search", name.map(|Path(n)| n), p).await
}

pub async fn delete_search_pipeline(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    delete_pipeline(store, "search", name, p).await
}
