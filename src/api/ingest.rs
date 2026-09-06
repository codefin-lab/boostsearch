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
    // an ingest pipeline is read now, so a processor that cannot run is
    // refused here rather than at the first document
    if kind == "ingest"
        && let Err(e) = crate::ingest::Pipeline::parse(&name, &body)
    {
        return ingest_failure(&e);
    }
    if kind == "search"
        && let Err(e) = crate::search::pipeline::Pipeline::parse(&name, &body)
    {
        return crate::api::pipeline_failure(&e);
    }
    store.put_pipeline(kind, &name, body);
    respond(&p, json!({"acknowledged": true}))
}

/// A `pipeline` processor naming a pipeline that is not there, anywhere in
/// a definition: refused when the definition is read.
fn missing_named_pipeline(
    store: &Store,
    specs: &[crate::ingest::ProcessorSpec],
) -> Option<crate::ingest::IngestError> {
    for spec in specs {
        if spec.kind == "pipeline"
            && let Some(name) = spec.config.get("name").and_then(|v| v.as_str())
            && !name.contains("{{")
            && !store.pipelines("ingest").contains_key(name)
            && !spec
                .config
                .get("ignore_missing_pipeline")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        {
            return Some(crate::ingest::IngestError::illegal(format!(
                "Pipeline processor configured for non-existent pipeline [{name}]"
            )));
        }
        if let Some(e) = missing_named_pipeline(store, &spec.on_failure) {
            return Some(e);
        }
    }
    None
}

/// An ingest error as a response.
pub(crate) fn ingest_failure(e: &crate::ingest::IngestError) -> Response {
    let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::BAD_REQUEST);
    (status, axum::Json(json!({"error": e.body(), "status": e.status()}))).into_response()
}

/// `_ingest/pipeline/_simulate` and `_ingest/pipeline/{id}/_simulate`: run
/// a pipeline over documents given in the request and report what each
/// became, processor by processor where `verbose` asks.
pub async fn simulate_pipeline(
    State(store): State<Store>,
    name: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    use crate::ingest::{IngestDoc, Pipeline, run_pipeline};
    let body: Value = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let verbose = p.get("verbose").map(|v| v != "false").unwrap_or(false)
        || body.get("verbose").and_then(|v| v.as_bool()).unwrap_or(false);
    let pipeline = match name.map(|Path(n)| n) {
        Some(n) => match store.pipelines("ingest").remove(&n) {
            Some(def) => match Pipeline::parse(&n, &def) {
                Ok(p) => p,
                Err(e) => return ingest_failure(&e),
            },
            None => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "illegal_argument_exception",
                    format!("pipeline [{n}] does not exist"),
                );
            }
        },
        None => match body.get("pipeline") {
            Some(def) => match Pipeline::parse("_simulate_pipeline", def) {
                Ok(p) => {
                    if let Some(e) = missing_named_pipeline(&store, &p.processors) {
                        return ingest_failure(&e);
                    }
                    p
                }
                Err(e) => return ingest_failure(&e),
            },
            None => {
                let e = crate::ingest::IngestError::parse(
                    "[pipeline] required property is missing",
                    None,
                    None,
                    Some("pipeline"),
                );
                return ingest_failure(&e);
            }
        },
    };
    let Some(docs) = body.get("docs").and_then(|d| d.as_array()) else {
        let e = crate::ingest::IngestError::parse(
            "[docs] required property is missing",
            None,
            None,
            Some("docs"),
        );
        return ingest_failure(&e);
    };
    let mut out = Vec::new();
    for d in docs {
        let index = d.get("_index").and_then(|v| v.as_str()).unwrap_or("_index").to_string();
        let id = d.get("_id").and_then(|v| v.as_str()).unwrap_or("_id").to_string();
        let mut doc = IngestDoc::new(&index, &id, d.get("_source").cloned().unwrap_or(json!({})));
        doc.routing = d.get("_routing").and_then(|v| v.as_str().map(|s| s.to_string()));
        if let Some(v) = d.get("_version") {
            match v.as_i64() {
                Some(n) => doc.version = Some(n),
                None => {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        "Failed to parse parameter [_version], only int or long is accepted",
                    );
                }
            }
        }
        doc.version_raw = d.get("_version").cloned();
        doc.version_type = d.get("_version_type").and_then(|v| v.as_str().map(|s| s.to_string()));
        if let Some(v) = d.get("_if_seq_no") {
            match v.as_i64() {
                Some(n) => doc.if_seq_no = Some(n),
                None => {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        "Failed to parse parameter [_if_seq_no], only int or long is accepted",
                    );
                }
            }
        }
        if let Some(v) = d.get("_if_primary_term") {
            match v.as_i64() {
                Some(n) => doc.if_primary_term = Some(n),
                None => {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "illegal_argument_exception",
                        "Failed to parse parameter [_if_primary_term], only int or long is accepted",
                    );
                }
            }
        }
        if verbose {
            doc.ingest.insert("pipeline".into(), json!(pipeline.name));
        }
        let mut steps = Vec::new();
        let mut depth = Vec::new();
        let result = run_pipeline(&store, &pipeline, doc, &mut steps, &mut depth);
        if verbose {
            let listed: Vec<Value> = steps
                .iter()
                .map(|s| {
                    let mut one = json!({"processor_type": s.processor_type});
                    if let Some(t) = &s.tag {
                        one["tag"] = json!(t);
                    }
                    if let Some(d) = &s.description {
                        one["description"] = json!(d);
                    }
                    if let Some(m) = s.condition_met {
                        one["if"] = json!({"condition": s.condition_text.clone().unwrap_or_default(), "result": m});
                    }
                    one["status"] = json!(s.status);
                    match (s.status, &s.error, &s.doc) {
                        ("error_ignored", Some(e), Some(d)) => {
                            one["ignored_error"] = json!({"error": e.body()});
                            one["doc"] = d.to_json();
                        }
                        ("error", Some(e), _) => {
                            one["error"] = e.body();
                        }
                        (_, _, Some(d)) => {
                            one["doc"] = d.to_json();
                        }
                        _ => {}
                    }
                    one
                })
                .collect();
            out.push(json!({"processor_results": listed}));
        } else {
            match result {
                Ok(Some(d)) => out.push(json!({"doc": d.to_json()})),
                Ok(None) => out.push(json!({"doc": Value::Null})),
                Err(e) => out.push(json!({"error": e.body()})),
            }
        }
    }
    respond(&p, json!({"docs": out}))
}

/// `_ingest/processor/grok`: the patterns grok knows by name.
pub async fn grok_patterns(Query(p): Query<Params>) -> Response {
    respond(&p, crate::ingest::grok::bank_json())
}

/// The pipelines a write runs: the one asked for, or the index's default,
/// then the index's final one. `_none` asks for none.
pub(crate) fn pipelines_for_write(store: &Store, index: &str, asked: Option<&str>) -> Vec<String> {
    // an alias stands for the index behind it
    let name = store.resolve(index).into_iter().next().unwrap_or_else(|| index.to_string());
    match store.get(&name) {
        Some(st) => pipelines_for_state(&st.read(), asked),
        None => {
            // an index not there yet gets its settings from the templates
            // that would make it
            let would_be = store.apply_templates(&name, &json!({}));
            let setting = |key: &str| -> Option<String> {
                would_be
                    .pointer(&format!("/settings/index/{key}"))
                    .or_else(|| would_be.pointer(&format!("/settings/index.{key}")))
                    .or_else(|| would_be.pointer(&format!("/settings/{key}")))
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .filter(|v| !v.is_empty() && v != "_none")
            };
            let mut out = Vec::new();
            match asked {
                Some("_none") => {}
                Some(named) => out.push(named.to_string()),
                None => {
                    if let Some(d) = setting("default_pipeline") {
                        out.push(d);
                    }
                }
            }
            if let Some(f) = setting("final_pipeline") {
                out.push(f);
            }
            out
        }
    }
}

/// The same, read off an index that is already held, where the store
/// says any pipeline exists at all.
pub(crate) fn pipelines_for_state_in(
    store: &Store,
    g: &IdxState,
    asked: Option<&str>,
) -> Vec<String> {
    if !store.any_ingest_pipeline.load(std::sync::atomic::Ordering::Relaxed) {
        return match asked {
            Some(named) if named != "_none" => vec![named.to_string()],
            _ => Vec::new(),
        };
    }
    pipelines_for_state(g, asked)
}

/// The same, read off an index that is already held.
pub(crate) fn pipelines_for_state(g: &IdxState, asked: Option<&str>) -> Vec<String> {
    let setting =
        |key: &str| -> Option<String> { g.setting(key).filter(|v| !v.is_empty() && v != "_none") };
    let mut out = Vec::new();
    match asked {
        Some("_none") => {}
        Some(named) => out.push(named.to_string()),
        None => {
            if let Some(d) =
                setting("default_pipeline").or_else(|| setting("index.default_pipeline"))
            {
                out.push(d);
            }
        }
    }
    if let Some(f) = setting("final_pipeline").or_else(|| setting("index.final_pipeline")) {
        out.push(f);
    }
    out
}

/// What a write said about the document besides its source.
#[derive(Default, Clone)]
pub(crate) struct WriteMeta {
    pub version: Option<i64>,
    pub version_type: Option<String>,
    pub if_seq_no: Option<i64>,
    pub if_primary_term: Option<i64>,
}

/// As `ingest_for_write`, with the version and conditions the request gave.
pub(crate) fn ingest_for_write_with(
    store: &Store,
    index: &str,
    id: &str,
    source: Value,
    asked: Option<&str>,
    routing: Option<String>,
    given: WriteMeta,
) -> std::result::Result<Option<crate::ingest::IngestDoc>, crate::ingest::IngestError> {
    use crate::ingest::IngestDoc;
    let names = pipelines_for_write(store, index, asked);
    let mut doc = IngestDoc::new(index, id, source);
    doc.routing = routing;
    doc.version = given.version;
    doc.version_type = given.version_type;
    doc.if_seq_no = given.if_seq_no;
    doc.if_primary_term = given.if_primary_term;
    if names.is_empty() {
        return Ok(Some(doc));
    }
    run_named_pipelines(store, names, doc)
}

/// Run the pipelines a write asks for over a document. `Ok(None)` means a
/// processor dropped it.
pub(crate) fn ingest_for_write(
    store: &Store,
    index: &str,
    id: &str,
    source: Value,
    asked: Option<&str>,
    routing: Option<String>,
) -> std::result::Result<Option<crate::ingest::IngestDoc>, crate::ingest::IngestError> {
    use crate::ingest::IngestDoc;
    let names = pipelines_for_write(store, index, asked);
    if names.is_empty() {
        return Ok(Some({
            let mut d = IngestDoc::new(index, id, source);
            d.routing = routing;
            d
        }));
    }
    let mut doc = IngestDoc::new(index, id, source);
    doc.routing = routing;
    run_named_pipelines(store, names, doc)
}

/// Whether any node in the cluster carries the ingest role.
pub(crate) fn any_ingest_node() -> bool {
    let mine = &crate::cluster::identity().roles;
    if mine.iter().any(|r| r == "ingest") {
        return true;
    }
    crate::cluster::current_state().nodes.values().any(|n| n.roles.iter().any(|r| r == "ingest"))
}

/// Run pipelines by name over a document, counting each run.
pub(crate) fn run_named_pipelines(
    store: &Store,
    names: Vec<String>,
    mut doc: crate::ingest::IngestDoc,
) -> std::result::Result<Option<crate::ingest::IngestDoc>, crate::ingest::IngestError> {
    use crate::ingest::{run_pipeline, stored_pipeline};
    // running a pipeline is the ingest role's work, and a cluster with no node
    // in that role has nowhere to send the document. The pipeline APIs still
    // answer -- one may be written, read and simulated on a node that will
    // never run it -- but a write that needs one cannot be carried out, and
    // that is true before it matters whether the pipeline exists.
    if !names.is_empty() && !any_ingest_node() {
        return Err(crate::ingest::IngestError::illegal(
            "There are no ingest nodes in this cluster, unable to forward request to an \
             ingest node.",
        ));
    }
    for name in names {
        let pipeline = match stored_pipeline(store, &name) {
            Some(Ok(p)) => p,
            Some(Err(e)) => return Err(e),
            None => {
                return Err(crate::ingest::IngestError::illegal(format!(
                    "pipeline with id [{name}] does not exist"
                )));
            }
        };
        let mut steps = Vec::new();
        let mut depth = Vec::new();
        doc.ingest.insert("pipeline".into(), json!(name));
        let started = std::time::Instant::now();
        let out = run_pipeline(store, &pipeline, doc, &mut steps, &mut depth);
        let took = started.elapsed().as_nanos() as u64;
        {
            let mut stats = store.ingest_stats.write();
            for key in [name.clone(), String::new()] {
                let e = stats.entry(key).or_insert((0, 0, 0));
                e.0 += 1;
                if out.is_err() {
                    e.1 += 1;
                }
                e.2 += took;
            }
        }
        match out {
            Ok(Some(d)) => doc = d,
            Ok(None) => return Ok(None),
            Err(e) => return Err(e),
        }
    }
    Ok(Some(doc))
}

/// The ingest section of the node's stats.
pub(crate) fn ingest_stats_json(store: &Store) -> Value {
    let stats = store.ingest_stats.read();
    let one = |v: &(u64, u64, u64)| json!({"count": v.0, "time_in_millis": v.2 / 1_000_000, "current": 0, "failed": v.1});
    let total = stats.get("").cloned().unwrap_or((0, 0, 0));
    let mut pipelines = serde_json::Map::new();
    for (name, v) in stats.iter() {
        if !name.is_empty() {
            pipelines.insert(name.clone(), one(v));
        }
    }
    json!({"total": one(&total), "pipelines": pipelines})
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
    if store.remove_pipelines(kind, &name) == 0 && name != "*" {
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
