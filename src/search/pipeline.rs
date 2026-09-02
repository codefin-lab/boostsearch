//! Search pipelines: what happens to a search request on the way in, and
//! to its answer on the way out.
//!
//! A pipeline names request processors, which change the search body
//! before it runs, and response processors, which change the hits after.
//! One may be named on the request, in the request, or by the index's
//! `index.search.default_pipeline`.

use serde_json::{Map, Value, json};

use crate::api::Params;
use crate::store::Store;

/// What a search pipeline could not do.
pub struct PipelineError {
    pub kind: String,
    pub reason: String,
    pub processor_type: Option<String>,
    pub processor_tag: Option<String>,
    pub property_name: Option<String>,
}

impl PipelineError {
    fn illegal(reason: impl Into<String>) -> PipelineError {
        PipelineError {
            kind: "illegal_argument_exception".into(),
            reason: reason.into(),
            processor_type: None,
            processor_tag: None,
            property_name: None,
        }
    }

    pub fn body(&self) -> Value {
        let mut c = json!({"type": self.kind, "reason": self.reason});
        if let Some(t) = &self.processor_type {
            c["processor_type"] = json!(t);
        }
        if let Some(t) = &self.processor_tag {
            c["processor_tag"] = json!(t);
        }
        if let Some(p) = &self.property_name {
            c["property_name"] = json!(p);
        }
        let mut top = c.clone();
        top["root_cause"] = json!([c]);
        top
    }

    pub fn status(&self) -> u16 {
        match self.kind.as_str() {
            "resource_not_found_exception" => 404,
            _ => 400,
        }
    }
}

pub const REQUEST_PROCESSORS: &[&str] = &["filter_query", "script", "oversample"];
pub const RESPONSE_PROCESSORS: &[&str] =
    &["rename_field", "sort", "truncate_hits", "collapse", "split", "personalize_search_ranking"];

/// One processor as written.
struct Spec {
    kind: String,
    config: Map<String, Value>,
    tag: Option<String>,
    ignore_failure: bool,
}

fn parse_list(list: &Value, allowed: &[&str], which: &str) -> Result<Vec<Spec>, PipelineError> {
    let Some(items) = list.as_array() else {
        return Err(PipelineError {
            kind: "parse_exception".into(),
            reason: format!("[{which}] property isn't a list, but of type [java.lang.String]"),
            processor_type: None,
            processor_tag: None,
            property_name: Some(which.into()),
        });
    };
    let mut out = Vec::new();
    for item in items {
        let Some(o) = item.as_object() else { continue };
        let Some((kind, cfg)) = o.iter().next() else { continue };
        let mut config = cfg.as_object().cloned().unwrap_or_default();
        let tag = config.remove("tag").and_then(|v| v.as_str().map(|s| s.to_string()));
        config.remove("description");
        let ignore_failure =
            config.remove("ignore_failure").and_then(|v| v.as_bool()).unwrap_or(false);
        if !allowed.contains(&kind.as_str()) {
            return Err(PipelineError {
                kind: "illegal_argument_exception".into(),
                reason: format!("Invalid processor type {kind}"),
                processor_type: Some(kind.clone()),
                processor_tag: tag.clone(),
                property_name: None,
            });
        }
        let spec = Spec { kind: kind.clone(), config, tag, ignore_failure };
        check(&spec, which)?;
        out.push(spec);
    }
    Ok(out)
}

fn check(spec: &Spec, _which: &str) -> Result<(), PipelineError> {
    let missing = |key: &str| PipelineError {
        kind: "parse_exception".into(),
        reason: format!("[{key}] required property is missing"),
        processor_type: Some(spec.kind.clone()),
        processor_tag: spec.tag.clone(),
        property_name: Some(key.into()),
    };
    match spec.kind.as_str() {
        "filter_query" => {
            let Some(q) = spec.config.get("query") else { return Err(missing("query")) };
            // a query of a kind nobody knows is refused where it is written
            if let Some(kind) = q.as_object().and_then(|o| o.keys().next())
                && crate::query::unknown_clause(kind)
            {
                return Err(PipelineError {
                    kind: "parsing_exception".into(),
                    reason: format!("unknown query [{kind}]"),
                    processor_type: None,
                    processor_tag: None,
                    property_name: None,
                });
            }
        }
        "script" => {
            let source = spec.config.get("source").and_then(|v| v.as_str()).unwrap_or("");
            if source.trim().is_empty() {
                return Err(PipelineError {
                    kind: "script_exception".into(),
                    reason: "compile error".into(),
                    processor_type: Some("script".into()),
                    processor_tag: spec.tag.clone(),
                    property_name: None,
                });
            }
            if let Err(e) = crate::painless::Script::compile(source) {
                return Err(PipelineError {
                    kind: "script_exception".into(),
                    reason: e.kind.to_string(),
                    processor_type: Some("script".into()),
                    processor_tag: spec.tag.clone(),
                    property_name: None,
                });
            }
        }
        "oversample" => {
            let f = spec.config.get("sample_factor").and_then(|v| v.as_f64()).unwrap_or(1.0);
            if f < 1.0 {
                return Err(PipelineError::illegal("sample_factor must be >= 1.0"));
            }
        }
        "rename_field" => {
            if spec.config.get("field").is_none() {
                return Err(missing("field"));
            }
            if spec.config.get("target_field").is_none() {
                return Err(missing("target_field"));
            }
        }
        "sort" => {
            if spec.config.get("field").is_none() {
                return Err(missing("field"));
            }
            if let Some(o) = spec.config.get("order").and_then(|v| v.as_str())
                && o != "asc"
                && o != "desc"
            {
                return Err(PipelineError::illegal(format!(
                    "Sort direction [{o}] not recognized. Valid values are: [asc, desc]"
                )));
            }
        }
        "collapse" => {
            if spec.config.get("field").is_none() {
                return Err(missing("field"));
            }
        }
        _ => {}
    }
    Ok(())
}

/// A pipeline read from its definition.
pub struct Pipeline {
    pub name: String,
    requests: Vec<Spec>,
    responses: Vec<Spec>,
}

impl Pipeline {
    pub fn parse(name: &str, def: &Value) -> Result<Pipeline, PipelineError> {
        let requests = match def.get("request_processors") {
            Some(list) => parse_list(list, REQUEST_PROCESSORS, "request_processors")?,
            None => Vec::new(),
        };
        let responses = match def.get("response_processors") {
            Some(list) => parse_list(list, RESPONSE_PROCESSORS, "response_processors")?,
            None => Vec::new(),
        };
        Ok(Pipeline { name: name.to_string(), requests, responses })
    }
}

/// The pipeline a search asks for, where it asks for one: a name on the
/// request or in the body, a definition in the body, or the index's default.
pub fn resolve(
    store: &Store,
    expr: &str,
    body: &Value,
    p: &Params,
) -> Result<Option<Pipeline>, PipelineError> {
    let named = p
        .get("search_pipeline")
        .cloned()
        .or_else(|| body.get("search_pipeline").and_then(|v| v.as_str()).map(|s| s.to_string()));
    if let Some(name) = named {
        if name == "_none" {
            return Ok(None);
        }
        return match store.pipelines("search").remove(&name) {
            Some(def) => Pipeline::parse(&name, &def).map(Some),
            None => Err(PipelineError {
                kind: "illegal_argument_exception".into(),
                reason: format!("Pipeline {name} could not be found"),
                processor_type: None,
                processor_tag: None,
                property_name: None,
            }),
        };
    }
    if let Some(def) = body.get("search_pipeline").filter(|v| v.is_object()) {
        return Pipeline::parse("_ad_hoc_pipeline", def).map(Some);
    }
    // the index's default, where every index asked agrees on one
    let mut found: Option<String> = None;
    for n in store.resolve(expr) {
        if let Some(st) = store.get(&n) {
            let g = st.read();
            if let Some(d) =
                g.setting("search.default_pipeline").filter(|d| !d.is_empty() && d != "_none")
            {
                match &found {
                    Some(f) if *f != d => return Ok(None),
                    _ => found = Some(d),
                }
            }
        }
    }
    match found {
        Some(name) => match store.pipelines("search").remove(&name) {
            Some(def) => Pipeline::parse(&name, &def).map(Some),
            None => Ok(None),
        },
        None => Ok(None),
    }
}

/// Take the pipeline's own keys out of a body before the search runs.
pub fn strip(body: &mut Value) {
    if let Some(o) = body.as_object_mut() {
        o.remove("search_pipeline");
    }
}

/// Apply the request processors to the search body.
pub fn before(
    store: &Store,
    pipeline: &Pipeline,
    body: &mut Value,
    context: &mut Map<String, Value>,
) -> Result<(), PipelineError> {
    for spec in &pipeline.requests {
        let out = request_step(store, spec, body, context);
        if let Err(e) = out {
            if spec.ignore_failure {
                continue;
            }
            return Err(e.tagged(spec));
        }
    }
    Ok(())
}

impl PipelineError {
    fn tagged(mut self, spec: &Spec) -> PipelineError {
        if self.processor_type.is_none() {
            self.processor_type = Some(spec.kind.clone());
        }
        if self.processor_tag.is_none() {
            self.processor_tag = spec.tag.clone();
        }
        self
    }
}

fn request_step(
    store: &Store,
    spec: &Spec,
    body: &mut Value,
    context: &mut Map<String, Value>,
) -> Result<(), PipelineError> {
    match spec.kind.as_str() {
        "filter_query" => {
            let filter = spec.config.get("query").cloned().unwrap_or(json!({"match_all": {}}));
            let existing = body.get("query").cloned();
            let combined = match existing {
                Some(q) => json!({"bool": {"must": [q], "filter": [filter]}}),
                None => json!({"bool": {"filter": [filter]}}),
            };
            body["query"] = combined;
        }
        "oversample" => {
            let factor = spec.config.get("sample_factor").and_then(|v| v.as_f64()).unwrap_or(1.0);
            let size = body.get("size").and_then(|v| v.as_i64()).unwrap_or(10);
            let prefix = spec
                .config
                .get("context_prefix")
                .and_then(|v| v.as_str())
                .map(|s| format!("{s}."))
                .unwrap_or_default();
            context.insert(format!("{prefix}original_size"), json!(size));
            body["size"] = json!((size as f64 * factor).ceil() as i64);
        }
        "script" => {
            let source = spec.config.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let script = crate::painless::Script::compile(source).map_err(|e| PipelineError {
                kind: "script_exception".into(),
                reason: e.kind.to_string(),
                processor_type: Some("script".into()),
                processor_tag: spec.tag.clone(),
                property_name: None,
            })?;
            // the script sees the search body as `ctx._source`, and a map it
            // may write into as `ctx.request_context`
            let mut source_map = body.as_object().cloned().unwrap_or_default();
            for (k, d) in [
                ("from", json!(-1)),
                ("size", json!(-1)),
                ("explain", json!(false)),
                ("version", json!(false)),
                ("seq_no_primary_term", json!(false)),
                ("track_scores", json!(false)),
                ("track_total_hits", json!(-1)),
                ("min_score", json!(0.0)),
                ("terminate_after", json!(0)),
                ("profile", json!(false)),
            ] {
                source_map.entry(k).or_insert(d);
            }
            let ctx = crate::painless::Value::map(vec![
                (
                    crate::painless::Value::str("_source"),
                    crate::painless::Value::from_json(&Value::Object(source_map)),
                ),
                (
                    crate::painless::Value::str("request_context"),
                    crate::painless::Value::from_json(&Value::Object(context.clone())),
                ),
            ]);
            let mut runner = crate::painless::contexts::Runner::new(
                &spec.config.get("params").cloned().unwrap_or(json!({})),
            )
            .with_ctx(ctx.clone());
            let _ = store;
            runner.run(&script).map_err(|e| PipelineError {
                kind: "script_exception".into(),
                reason: e.message,
                processor_type: Some("script".into()),
                processor_tag: spec.tag.clone(),
                property_name: None,
            })?;
            let back = ctx.to_json();
            if let Some(Value::Object(src)) = back.get("_source") {
                let mut next = src.clone();
                // what the defaults stood for is written back only where it
                // now says something
                if next.get("from").and_then(|v| v.as_i64()) == Some(-1) {
                    next.remove("from");
                }
                if next.get("size").and_then(|v| v.as_i64()) == Some(-1) {
                    next.remove("size");
                }
                if next.get("track_total_hits").and_then(|v| v.as_i64()) == Some(-1) {
                    next.remove("track_total_hits");
                }
                if next.get("min_score").and_then(|v| v.as_f64()) == Some(0.0) {
                    next.remove("min_score");
                }
                if next.get("terminate_after").and_then(|v| v.as_i64()) == Some(0) {
                    next.remove("terminate_after");
                }
                for k in ["explain", "version", "seq_no_primary_term", "track_scores", "profile"] {
                    if next.get(k) == Some(&json!(false)) {
                        next.remove(k);
                    }
                }
                *body = Value::Object(next);
            }
            if let Some(Value::Object(rc)) = back.get("request_context") {
                *context = rc.clone();
            }
        }
        _ => {}
    }
    Ok(())
}

/// Apply the response processors to the answer.
pub fn after(
    pipeline: &Pipeline,
    env: &mut Value,
    context: &Map<String, Value>,
) -> Result<(), PipelineError> {
    for spec in &pipeline.responses {
        if let Err(e) = response_step(spec, env, context) {
            if spec.ignore_failure {
                continue;
            }
            return Err(e.tagged(spec));
        }
    }
    Ok(())
}

fn response_step(
    spec: &Spec,
    env: &mut Value,
    context: &Map<String, Value>,
) -> Result<(), PipelineError> {
    let hits = match env.pointer_mut("/hits/hits").and_then(|h| h.as_array_mut()) {
        Some(h) => h,
        None => return Ok(()),
    };
    match spec.kind.as_str() {
        "rename_field" => {
            let field = spec.config.get("field").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let target =
                spec.config.get("target_field").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let ignore_missing =
                spec.config.get("ignore_missing").and_then(|v| v.as_bool()).unwrap_or(false);
            for hit in hits.iter_mut() {
                let id = hit.get("_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let mut found = false;
                for section in ["_source", "fields"] {
                    let Some(src) = hit.get_mut(section).and_then(|s| s.as_object_mut()) else {
                        continue;
                    };
                    if let Some(v) = src.remove(&field) {
                        src.insert(target.clone(), v);
                        found = true;
                    }
                }
                if !found && !ignore_missing {
                    return Err(PipelineError::illegal(format!(
                        "Document with id {id} is missing field {field}"
                    )));
                }
            }
        }
        "sort" => {
            let field = spec.config.get("field").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let target = spec
                .config
                .get("target_field")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| field.clone());
            let desc = spec.config.get("order").and_then(|v| v.as_str()) == Some("desc");
            for hit in hits.iter_mut() {
                let mut found = false;
                for section in ["_source", "fields"] {
                    let Some(src) = hit.get_mut(section).and_then(|s| s.as_object_mut()) else {
                        continue;
                    };
                    match src.get(&field).cloned() {
                        Some(Value::Array(mut a)) => {
                            a.sort_by(|x, y| {
                                x.as_f64()
                                    .partial_cmp(&y.as_f64())
                                    .unwrap_or(std::cmp::Ordering::Equal)
                                    .then_with(|| x.to_string().cmp(&y.to_string()))
                            });
                            if desc {
                                a.reverse();
                            }
                            src.insert(target.clone(), Value::Array(a));
                            found = true;
                        }
                        Some(_) => {
                            return Err(PipelineError::illegal(format!(
                                "field [{field}] of type [java.lang.String] cannot be cast to [java.util.List]"
                            )));
                        }
                        None => {}
                    }
                }
                if !found {
                    return Err(PipelineError::illegal(format!("field [{field}] doesn't exist")));
                }
            }
        }
        "truncate_hits" => {
            let prefix = spec
                .config
                .get("context_prefix")
                .and_then(|v| v.as_str())
                .map(|s| format!("{s}."))
                .unwrap_or_default();
            let size = spec.config.get("target_size").and_then(|v| v.as_i64()).or_else(|| {
                context.get(&format!("{prefix}original_size")).and_then(|v| v.as_i64())
            });
            if let Some(n) = size {
                hits.truncate(n.max(0) as usize);
            }
        }
        "collapse" => {
            let field = spec.config.get("field").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let mut seen: Vec<Value> = Vec::new();
            hits.retain(|hit| {
                let key = hit
                    .pointer(&format!("/_source/{}", field.replace('.', "/")))
                    .cloned()
                    .unwrap_or(Value::Null);
                if seen.contains(&key) {
                    false
                } else {
                    seen.push(key);
                    true
                }
            });
        }
        _ => {}
    }
    Ok(())
}
