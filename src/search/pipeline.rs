//! Search pipelines: what happens to a search request on the way in, to the
//! shards' results between the query and the page, and to its answer on the
//! way out.
//!
//! A pipeline names request processors, which change the search body
//! before it runs, phase results processors, which put a `hybrid` query's
//! scores together, and response processors, which change the hits after.
//! One may be named on the request, in the request, or by the index's
//! `index.search.default_pipeline`.

use serde_json::{Map, Value, json};

use crate::api::Params;
use crate::search::hybrid::Scoring;
use crate::store::Store;

/// What a search pipeline could not do.
pub struct PipelineError {
    pub kind: String,
    pub reason: String,
    pub processor_type: Option<String>,
    pub processor_tag: Option<String>,
    pub property_name: Option<String>,
    pub caused_by: Option<Value>,
}

impl PipelineError {
    pub(crate) fn of(kind: &str, reason: impl Into<String>) -> PipelineError {
        PipelineError {
            kind: kind.into(),
            reason: reason.into(),
            processor_type: None,
            processor_tag: None,
            property_name: None,
            caused_by: None,
        }
    }

    fn illegal(reason: impl Into<String>) -> PipelineError {
        PipelineError::of("illegal_argument_exception", reason)
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
        if let Some(cause) = &self.caused_by {
            top["caused_by"] = cause.clone();
        }
        top
    }

    pub fn status(&self) -> u16 {
        match self.kind.as_str() {
            "resource_not_found_exception" => 404,
            // what the reference fails on in its own code rather than in the
            // request: a cast, a null, a state it did not expect
            "class_cast_exception" | "null_pointer_exception" | "illegal_state_exception" => 500,
            _ => 400,
        }
    }
}

pub const REQUEST_PROCESSORS: &[&str] =
    &["filter_query", "neural_query_enricher", "oversample", "script"];
pub const RESPONSE_PROCESSORS: &[&str] = &[
    "collapse",
    "hybrid_score_explanation",
    "rename_field",
    "rerank",
    "sort",
    "split",
    "truncate_hits",
];
pub const PHASE_RESULTS_PROCESSORS: &[&str] =
    &["normalization-processor", "score-ranker-processor"];

/// The name Java gives a JSON value's type, which is how the reference
/// describes a property of the wrong kind.
fn java_type(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "java.lang.String",
        Value::Bool(_) => "java.lang.Boolean",
        Value::Number(n) if n.is_f64() => "java.lang.Double",
        Value::Number(n) if n.as_i64().is_some_and(|i| i32::try_from(i).is_ok()) => {
            "java.lang.Integer"
        }
        Value::Number(_) => "java.lang.Long",
        Value::Array(_) => "java.util.ArrayList",
        Value::Object(_) => "java.util.HashMap",
        Value::Null => "null",
    }
}

/// A float the way Java prints one: `1.0`, not `1`.
pub(crate) fn java_float(f: f32) -> String {
    let s = f.to_string();
    if s.contains(['.', 'e', 'E']) || !f.is_finite() { s } else { format!("{s}.0") }
}

/// A value's text the way `toString` gives it, for messages.
fn java_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => format!("[{}]", a.iter().map(java_text).collect::<Vec<_>>().join(", ")),
        other => other.to_string(),
    }
}

/// A processor's configuration, read one property at a time the way
/// OpenSearch's `ConfigurationUtils` reads it: each property is taken out as
/// it is read, so what is left at the end is what the processor does not take.
pub(crate) struct Config<'a> {
    kind: Option<String>,
    tag: Option<String>,
    map: &'a mut Map<String, Value>,
}

impl<'a> Config<'a> {
    fn new(kind: Option<&str>, tag: Option<String>, map: &'a mut Map<String, Value>) -> Config<'a> {
        Config { kind: kind.map(str::to_string), tag, map }
    }

    /// A map inside this configuration, read with the same processor named.
    pub(crate) fn nested<'b>(&self, map: &'b mut Map<String, Value>) -> Config<'b> {
        Config { kind: self.kind.clone(), tag: self.tag.clone(), map }
    }

    fn error(&self, key: &str, reason: impl std::fmt::Display) -> PipelineError {
        PipelineError {
            kind: "parse_exception".into(),
            reason: format!("[{key}] {reason}"),
            processor_type: self.kind.clone(),
            processor_tag: self.tag.clone(),
            property_name: Some(key.into()),
            caused_by: None,
        }
    }

    fn take(&mut self, key: &str) -> Option<Value> {
        self.map.remove(key).filter(|v| !v.is_null())
    }

    pub(crate) fn opt_string(&mut self, key: &str) -> Result<Option<String>, PipelineError> {
        match self.take(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(other) => Err(self.error(
                key,
                format!("property isn't a string, but of type [{}]", java_type(&other)),
            )),
        }
    }

    fn string(&mut self, key: &str) -> Result<String, PipelineError> {
        self.opt_string(key)?.ok_or_else(|| self.error(key, "required property is missing"))
    }

    pub(crate) fn string_or(&mut self, key: &str, default: &str) -> Result<String, PipelineError> {
        Ok(self.opt_string(key)?.unwrap_or_else(|| default.to_string()))
    }

    fn boolean(&mut self, key: &str, default: bool) -> Result<bool, PipelineError> {
        match self.take(key) {
            None => Ok(default),
            Some(Value::Bool(b)) => Ok(b),
            Some(other) => Err(self.error(
                key,
                format!("property isn't a boolean, but of type [{}]", java_type(&other)),
            )),
        }
    }

    fn opt_int(&mut self, key: &str) -> Result<Option<i64>, PipelineError> {
        let Some(v) = self.take(key) else { return Ok(None) };
        let text = java_text(&v);
        text.parse::<i32>().map(|n| Some(n as i64)).map_err(|_| {
            self.error(key, format!("property cannot be converted to an int [{text}]"))
        })
    }

    fn double(&mut self, key: &str) -> Result<f64, PipelineError> {
        let Some(v) = self.take(key) else {
            return Err(self.error(key, "required property is missing"));
        };
        let text = java_text(&v);
        text.trim().parse::<f64>().map_err(|_| {
            self.error(key, format!("property cannot be converted to a double [{text}]"))
        })
    }

    pub(crate) fn opt_map(
        &mut self,
        key: &str,
    ) -> Result<Option<Map<String, Value>>, PipelineError> {
        match self.take(key) {
            None => Ok(None),
            Some(Value::Object(m)) => Ok(Some(m)),
            Some(other) => Err(self
                .error(key, format!("property isn't a map, but of type [{}]", java_type(&other)))),
        }
    }
}

/// One processor as written.
struct Spec {
    kind: String,
    config: Map<String, Value>,
    tag: Option<String>,
    ignore_failure: bool,
}

/// Read one of a pipeline's processor lists.
fn read_list(
    top: &mut Map<String, Value>,
    which: &str,
    allowed: &[&str],
    scorings: &mut Vec<Scoring>,
) -> Result<Vec<Spec>, PipelineError> {
    let mut outer = Config::new(None, None, top);
    let list = match outer.take(which) {
        None => return Ok(Vec::new()),
        Some(Value::Array(a)) => a,
        Some(other) => {
            return Err(outer.error(
                which,
                format!("property isn't a list, but of type [{}]", java_type(&other)),
            ));
        }
    };
    let cast = |v: &Value| {
        let from = java_type(v);
        PipelineError::of(
            "class_cast_exception",
            format!(
                "class {from} cannot be cast to class java.util.Map ({from} and java.util.Map are \
                 in module java.base of loader 'bootstrap')"
            ),
        )
    };
    let mut out = Vec::new();
    for item in list {
        let Value::Object(item) = item else { return Err(cast(&item)) };
        // an object may name more than one processor, and each one counts
        for (kind, cfg) in item {
            if !allowed.contains(&kind.as_str()) {
                return Err(PipelineError::illegal(format!("Invalid processor type {kind}")));
            }
            let mut config = match cfg {
                Value::Object(m) => m,
                Value::Null => {
                    return Err(PipelineError::of(
                        "null_pointer_exception",
                        "Cannot invoke \"java.util.Map.remove(Object)\" because \"configuration\" is null",
                    ));
                }
                other => return Err(cast(&other)),
            };
            let mut common = Config::new(None, None, &mut config);
            let tag = common.opt_string("tag")?;
            let ignore_failure = common.boolean("ignore_failure", false)?;
            let mut common = Config::new(None, tag.clone(), &mut config);
            common.opt_string("description")?;
            let mut left = config.clone();
            let mut cfg = Config::new(Some(&kind), tag.clone(), &mut left);
            check(&kind, &mut cfg, scorings)?;
            if !left.is_empty() {
                let named = match &tag {
                    Some(t) => format!("{kind}:{t}"),
                    None => kind.clone(),
                };
                return Err(PipelineError::of(
                    "parse_exception",
                    format!(
                        "processor [{named}] doesn't support one or more provided configuration \
                         parameters: [{}]",
                        left.keys().cloned().collect::<Vec<_>>().join(", ")
                    ),
                ));
            }
            out.push(Spec { kind, config, tag, ignore_failure });
        }
    }
    Ok(out)
}

/// Read what a processor takes, refusing what its factory refuses.
fn check(kind: &str, cfg: &mut Config, scorings: &mut Vec<Scoring>) -> Result<(), PipelineError> {
    match kind {
        "filter_query" => {
            let Some(q) = cfg.opt_map("query")? else {
                return Err(PipelineError::illegal(
                    "Did not specify the query property in processor of type filter_query",
                ));
            };
            // a query of a kind nobody knows is refused where it is written
            if let Some(kind) = q.keys().next()
                && crate::query::unknown_clause(kind)
            {
                let mut e =
                    PipelineError::of("parsing_exception", format!("unknown query [{kind}]"));
                // the parser reads the query from its own text, where the name
                // ends at this column
                e.caused_by = Some(json!({
                    "type": "named_object_not_found_exception",
                    "reason": format!("[1:{}] unknown field [{kind}]", kind.chars().count() + 5),
                }));
                return Err(e);
            }
        }
        "script" => {
            let mut script = Map::new();
            for key in ["id", "source", "inline", "lang", "params", "options"] {
                if let Some(v) = cfg.take(key) {
                    script.insert(key.into(), v);
                }
            }
            let source = script.get("source").or_else(|| script.get("inline"));
            if source.is_none() && script.get("id").is_none() {
                return Err(PipelineError::illegal(
                    "must specify either [source] for an inline script or [id] for a stored script",
                ));
            }
            if let Some(lang) = script.get("lang").and_then(|v| v.as_str())
                && lang != "painless"
            {
                return Err(PipelineError::illegal(format!(
                    "{lang} engine does not know how to handle context [search]"
                )));
            }
            let source = source.and_then(|v| v.as_str()).unwrap_or("");
            if source.trim().is_empty() {
                return Err(script_error("compile error", cfg.tag.clone()));
            }
            if let Err(e) = crate::painless::Script::compile(source) {
                return Err(script_error(e.kind, cfg.tag.clone()));
            }
        }
        "oversample" => {
            if cfg.double("sample_factor")? < 1.0 {
                return Err(cfg.error("sample_factor", "Value must be >= 1.0"));
            }
            cfg.opt_string("context_prefix")?;
        }
        "rename_field" => {
            cfg.string("field")?;
            cfg.string("target_field")?;
            cfg.boolean("ignore_missing", false)?;
        }
        "sort" => {
            let field = cfg.string("field")?;
            cfg.string_or("target_field", &field)?;
            let order = cfg.string_or("order", "asc")?;
            if order != "asc" && order != "desc" {
                return Err(cfg.error(
                    "order",
                    format!(
                        "Sort direction [{order}] not recognized. Valid values are: [asc, desc]"
                    ),
                ));
            }
        }
        "collapse" => {
            cfg.string("field")?;
        }
        "truncate_hits" => {
            if cfg.opt_int("target_size")?.is_some_and(|n| n < 0) {
                return Err(cfg.error("target_size", "Value must be >= 0"));
            }
            cfg.opt_string("context_prefix")?;
        }
        "split" => {
            let field = cfg.string("field")?;
            cfg.string("separator")?;
            cfg.boolean("preserve_trailing", false)?;
            cfg.string_or("target_field", &field)?;
        }
        "rerank" => {
            let mut types: Vec<&str> = ["by_field", "ml_opensearch"]
                .into_iter()
                .filter(|t| cfg.map.contains_key(*t))
                .collect();
            types.sort();
            match types.as_slice() {
                [] => {
                    return Err(PipelineError::illegal(
                        "No rerank type found. Possible rerank types are: [ml_opensearch, by_field]",
                    ));
                }
                ["by_field"] => {
                    let mut by = cfg.opt_map("by_field")?.unwrap_or_default();
                    let mut inner = cfg.nested(&mut by);
                    inner.string("target_field")?;
                    inner.boolean("remove_target_field", false)?;
                    inner.boolean("keep_previous_score", false)?;
                }
                [_] => {
                    // a rerank by a model needs the model, and none runs here
                    return Err(PipelineError::illegal(
                        "rerank type [ml_opensearch] needs a model, and this server runs none",
                    ));
                }
                _ => {
                    return Err(PipelineError::illegal(format!(
                        "Multiple rerank types found: [{}]. Only one is permitted.",
                        types.join(", ")
                    )));
                }
            }
        }
        "neural_query_enricher" => {
            let model = cfg.opt_string("default_model_id")?;
            let fields = cfg.opt_map("neural_field_default_id")?;
            if model.is_none() && fields.is_none() {
                return Err(PipelineError::illegal(
                    "model Id or neural info map either of them should be provided",
                ));
            }
        }
        "hybrid_score_explanation" => {}
        "normalization-processor" => {
            scorings.push(crate::search::hybrid::parse_normalization(cfg)?)
        }
        "score-ranker-processor" => scorings.push(crate::search::hybrid::parse_score_ranker(cfg)?),
        _ => {}
    }
    Ok(())
}

fn script_error(reason: impl Into<String>, tag: Option<String>) -> PipelineError {
    PipelineError {
        processor_type: Some("script".into()),
        processor_tag: tag,
        ..PipelineError::of("script_exception", reason)
    }
}

/// A pipeline read from its definition.
pub struct Pipeline {
    pub name: String,
    requests: Vec<Spec>,
    responses: Vec<Spec>,
    /// how a hybrid query's scores are put together, from the first phase
    /// results processor
    scoring: Option<Scoring>,
}

impl Pipeline {
    pub fn parse(store: &Store, name: &str, def: &Value) -> Result<Pipeline, PipelineError> {
        let mut top = def.as_object().cloned().unwrap_or_default();
        inline_stored_scripts(store, &mut top)?;
        let mut cfg = Config::new(None, None, &mut top);
        cfg.opt_string("description")?;
        cfg.opt_int("version")?;
        let mut scorings = Vec::new();
        let requests =
            read_list(&mut top, "request_processors", REQUEST_PROCESSORS, &mut scorings)?;
        let responses =
            read_list(&mut top, "response_processors", RESPONSE_PROCESSORS, &mut scorings)?;
        read_list(&mut top, "phase_results_processors", PHASE_RESULTS_PROCESSORS, &mut scorings)?;
        if !top.is_empty() {
            return Err(PipelineError::of(
                "parse_exception",
                format!(
                    "pipeline [{name}] doesn't support one or more provided configuration \
                     parameters [{}]",
                    top.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
            ));
        }
        Ok(Pipeline {
            name: name.to_string(),
            requests,
            responses,
            scoring: scorings.into_iter().next(),
        })
    }

    /// How this pipeline scores a hybrid query, if it says.
    pub(crate) fn scoring(&self) -> Option<&Scoring> {
        self.scoring.as_ref()
    }
}

/// A `script` request processor may name a stored script instead of writing
/// one: it is read in here, so the processor is checked and run with the text
/// the name stands for, and a name nothing is stored under is refused.
fn inline_stored_scripts(store: &Store, top: &mut Map<String, Value>) -> Result<(), PipelineError> {
    let Some(list) = top.get_mut("request_processors").and_then(|l| l.as_array_mut()) else {
        return Ok(());
    };
    for item in list {
        let Some(script) = item.get_mut("script").and_then(|s| s.as_object_mut()) else {
            continue;
        };
        if script.contains_key("source") || script.contains_key("inline") {
            continue;
        }
        let Some(id) = script.get("id").and_then(|v| v.as_str()).map(str::to_string) else {
            continue;
        };
        let Some(found) = store.stored_script(&id) else {
            return Err(PipelineError::of(
                "resource_not_found_exception",
                format!("unable to find script [{id}] in cluster state"),
            ));
        };
        script.remove("id");
        for key in ["source", "lang"] {
            if let Some(v) = found.get(key)
                && !script.contains_key(key)
            {
                script.insert(key.into(), v.clone());
            }
        }
    }
    Ok(())
}

fn not_defined(name: &str) -> PipelineError {
    PipelineError::illegal(format!("Pipeline {name} is not defined"))
}

/// The pipeline a search asks for, where it asks for one: a name on the
/// request or in the body, a definition in the body, or the index's default.
pub fn resolve(
    store: &Store,
    expr: &str,
    body: &Value,
    p: &Params,
) -> Result<Option<Pipeline>, PipelineError> {
    let on_url = p.get("search_pipeline").cloned();
    // a definition in the body stands alone: naming a pipeline as well is a
    // request that says two things, and neither wins
    if let Some(def) = body.get("search_pipeline").filter(|v| v.is_object()) {
        if on_url.is_some() {
            return Err(PipelineError::illegal(
                "Both named and inline search pipeline were specified. Please only specify one \
                 or the other.",
            ));
        }
        return Pipeline::parse(store, "_ad_hoc_pipeline", def).map(Some);
    }
    let named = on_url
        .or_else(|| body.get("search_pipeline").and_then(|v| v.as_str()).map(|s| s.to_string()));
    let mut chosen = "_none".to_string();
    if let Some(name) = named {
        chosen = name;
    } else if !expr.trim().is_empty() {
        // the indices' defaults, read in turn: the first sets it, and one
        // that disagrees with it puts it back to none
        for n in store.resolve(expr) {
            let Some(st) = store.get(&n) else { continue };
            let Some(d) = st.read().setting("search.default_pipeline").filter(|d| !d.is_empty())
            else {
                continue;
            };
            if chosen == "_none" {
                chosen = d;
            } else if chosen != d {
                chosen = "_none".to_string();
                break;
            }
        }
    }
    if chosen == "_none" {
        return Ok(None);
    }
    match store.pipelines("search").remove(&chosen) {
        Some(def) => Pipeline::parse(store, &chosen, &def).map(Some),
        None => Err(not_defined(&chosen)),
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
            // a failure while running is the processor's own exception, not
            // one that names the processor: only a script's says which it was
            return Err(e);
        }
    }
    Ok(())
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
            let factor = spec
                .config
                .get("sample_factor")
                .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
                .unwrap_or(1.0);
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
            let source = spec
                .config
                .get("source")
                .or_else(|| spec.config.get("inline"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let script = crate::painless::Script::compile(source)
                .map_err(|e| script_error(e.kind, spec.tag.clone()))?;
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
            runner.run(&script).map_err(|e| script_error(e.message, spec.tag.clone()))?;
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
        // there is no neural query here for it to fill a model into
        _ => {}
    }
    Ok(())
}

/// Apply the response processors to the answer.
pub fn after(
    pipeline: &Pipeline,
    body: &Value,
    env: &mut Value,
    context: &Map<String, Value>,
) -> Result<(), PipelineError> {
    for spec in &pipeline.responses {
        if let Err(e) = response_step(spec, body, env, context) {
            if spec.ignore_failure {
                continue;
            }
            return Err(e);
        }
    }
    Ok(())
}

fn text_of(spec: &Spec, key: &str) -> String {
    spec.config.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn flag_of(spec: &Spec, key: &str) -> bool {
    spec.config.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// The reference rebuilds the answer around a new set of hits, and an answer
/// rebuilt that way carries an empty profile.
fn rebuilt(env: &mut Value) {
    if env.get("profile").is_none() {
        env["profile"] = json!({"shards": []});
    }
}

fn response_step(
    spec: &Spec,
    body: &Value,
    env: &mut Value,
    context: &Map<String, Value>,
) -> Result<(), PipelineError> {
    if env.pointer("/hits/hits").and_then(|h| h.as_array()).is_none() {
        return Ok(());
    }
    match spec.kind.as_str() {
        "rename_field" => {
            let field = text_of(spec, "field");
            let target = text_of(spec, "target_field");
            let ignore_missing = flag_of(spec, "ignore_missing");
            let hits = env["hits"]["hits"].as_array_mut().into_iter().flatten();
            // once any hit has had the field, a later one without it is let
            // be, as the reference's flag is never set back
            let mut found = false;
            for hit in hits {
                let id = hit.get("_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                for section in ["fields", "_source"] {
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
            let field = text_of(spec, "field");
            let target = spec
                .config
                .get("target_field")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| field.clone());
            let desc = spec.config.get("order").and_then(|v| v.as_str()) == Some("desc");
            for hit in env["hits"]["hits"].as_array_mut().into_iter().flatten() {
                if let Some(fields) = hit.get_mut("fields").and_then(|s| s.as_object_mut())
                    && let Some(values) = fields.get(&field)
                {
                    let Some(values) = values.as_array() else {
                        return Err(PipelineError::illegal(format!(
                            "field [{field}] is null, cannot sort."
                        )));
                    };
                    let sorted = sorted_values(&field, values, desc)?;
                    fields.insert(target.clone(), Value::Array(sorted));
                }
                if let Some(src) = hit.get_mut("_source").and_then(|s| s.as_object_mut())
                    && let Some(Value::Array(values)) = src.get(&field)
                {
                    let sorted = sorted_values(&field, values, desc)?;
                    src.insert(target.clone(), Value::Array(sorted));
                }
            }
        }
        "split" => {
            let field = text_of(spec, "field");
            let target = spec
                .config
                .get("target_field")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| field.clone());
            let separator = text_of(spec, "separator");
            let keep_trailing = flag_of(spec, "preserve_trailing");
            let pattern = regex::Regex::new(&separator)
                .map_err(|e| PipelineError::of("pattern_syntax_exception", e.to_string()))?;
            let split = |s: &str| {
                let mut parts: Vec<&str> = pattern.split(s).collect();
                // Java's `split` drops the empty strings at the end unless
                // told to keep them
                if !keep_trailing && !s.is_empty() {
                    while parts.len() > 1 && parts.last() == Some(&"") {
                        parts.pop();
                    }
                    if parts == [""] {
                        parts.clear();
                    }
                }
                Value::Array(parts.into_iter().map(|p| json!(p)).collect())
            };
            for hit in env["hits"]["hits"].as_array_mut().into_iter().flatten() {
                if let Some(fields) = hit.get_mut("fields").and_then(|s| s.as_object_mut())
                    && let Some(values) = fields.get(&field)
                {
                    let Some(text) = values.get(0).and_then(|v| v.as_str()) else {
                        return Err(PipelineError::illegal(format!(
                            "field [{field}] is not a string, cannot split"
                        )));
                    };
                    let parts = split(text);
                    fields.insert(target.clone(), parts);
                }
                if let Some(src) = hit.get_mut("_source").and_then(|s| s.as_object_mut())
                    && let Some(Value::String(text)) = src.get(&field)
                {
                    let parts = split(text);
                    src.insert(target.clone(), parts);
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
            let size = spec
                .config
                .get("target_size")
                .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
                .or_else(|| {
                    context.get(&format!("{prefix}original_size")).and_then(|v| v.as_i64())
                });
            let Some(n) = size else {
                return Err(PipelineError::of(
                    "illegal_state_exception",
                    format!(
                        "Must specify target_size unless an earlier processor set {prefix}original_size"
                    ),
                ));
            };
            let hits = env["hits"]["hits"].as_array_mut().expect("hits were checked");
            if hits.len() > n.max(0) as usize {
                hits.truncate(n.max(0) as usize);
                rebuilt(env);
            }
        }
        "collapse" => {
            let field = text_of(spec, "field");
            if let Some(already) = body.pointer("/collapse/field").and_then(|f| f.as_str()) {
                return Err(PipelineError::of(
                    "illegal_state_exception",
                    format!("Cannot collapse on {field}. Results already collapsed on {already}"),
                ));
            }
            let mut seen: Vec<String> = Vec::new();
            let mut kept = Vec::new();
            let hits = env["hits"]["hits"].as_array_mut().expect("hits were checked");
            for hit in std::mem::take(hits) {
                let from_fields =
                    hit.get("fields").and_then(|f| f.get(&field)).and_then(|v| v.as_array());
                let value = match from_fields {
                    Some(values) if values.len() > 1 => {
                        let id = hit.get("_id").and_then(|v| v.as_str()).unwrap_or("");
                        return Err(PipelineError::of(
                            "illegal_state_exception",
                            format!(
                                "Failed to collapse {id}: doc has multiple values for field {field}"
                            ),
                        ));
                    }
                    Some(values) => values.first().cloned(),
                    None => hit.get("_source").and_then(|s| s.get(&field)).cloned(),
                };
                let key = match value {
                    None | Some(Value::Null) => "__missing__".to_string(),
                    Some(v) => java_text(&v),
                };
                if !seen.contains(&key) {
                    seen.push(key);
                    kept.push(hit);
                }
            }
            env["hits"]["hits"] = Value::Array(kept);
            rebuilt(env);
        }
        "rerank" => {
            let Some(by) = spec.config.get("by_field") else { return Ok(()) };
            let target = by.get("target_field").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let remove = by.get("remove_target_field").and_then(|v| v.as_bool()).unwrap_or(false);
            let keep = by.get("keep_previous_score").and_then(|v| v.as_bool()).unwrap_or(false);
            let path: Vec<&str> = target.split('.').collect();
            let hits = env["hits"]["hits"].as_array_mut().expect("hits were checked");
            if hits.is_empty() {
                return Ok(());
            }
            for hit in hits.iter_mut() {
                let id = hit.get("_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let previous = hit.get("_score").cloned().unwrap_or(Value::Null);
                let Some(src) = hit.get_mut("_source").filter(|s| s.is_object()) else {
                    return Err(PipelineError::illegal(format!(
                        "There is no source field to be able to perform rerank on hit [{id}]"
                    )));
                };
                let Some(value) = path.iter().try_fold(&*src, |at, part| at.get(*part)).cloned()
                else {
                    return Err(PipelineError::illegal(format!(
                        "The field to rerank by is not found at hit [{id}]"
                    )));
                };
                let Some(score) = value.as_f64() else {
                    let kind = match &value {
                        Value::String(_) => "String",
                        Value::Bool(_) => "Boolean",
                        Value::Array(_) => "ArrayList",
                        Value::Object(_) => "LinkedHashMap",
                        _ => "Object",
                    };
                    return Err(PipelineError::illegal(format!(
                        "The field mapping to rerank by [{}] is not Numerical, instead of type [{kind}]",
                        java_text(&value)
                    )));
                };
                if remove {
                    remove_path(src, &path);
                }
                if keep {
                    src["previous_score"] = previous;
                }
                hit["_score"] = json!(score as f32);
            }
            hits.sort_by(|a, b| {
                let s = |h: &Value| h.get("_score").and_then(|v| v.as_f64()).unwrap_or(0.0);
                s(b).total_cmp(&s(a))
            });
            let best = hits.first().and_then(|h| h.get("_score")).cloned().unwrap_or(Value::Null);
            env["hits"]["max_score"] = best;
            rebuilt(env);
        }
        _ => {}
    }
    Ok(())
}

/// Take a dotted path out of a source, and the objects it leaves empty.
fn remove_path(at: &mut Value, path: &[&str]) {
    let Some((first, rest)) = path.split_first() else { return };
    let Some(o) = at.as_object_mut() else { return };
    if rest.is_empty() {
        o.remove(*first);
        return;
    }
    if let Some(inner) = o.get_mut(*first) {
        remove_path(inner, rest);
        if inner.as_object().is_some_and(|m| m.is_empty()) {
            o.remove(*first);
        }
    }
}

/// A list's values in order, compared the way Java compares what it can.
fn sorted_values(field: &str, values: &[Value], desc: bool) -> Result<Vec<Value>, PipelineError> {
    for v in values {
        match v {
            Value::Null => {
                return Err(PipelineError::illegal(format!(
                    "field [{field}] contains a null value.]"
                )));
            }
            Value::Array(_) | Value::Object(_) => {
                return Err(PipelineError::illegal(format!(
                    "field [{field}] of type [{}] is not comparable.]",
                    java_type(v)
                )));
            }
            _ => {}
        }
    }
    let mut a = values.to_vec();
    a.sort_by(|x, y| match (x.as_f64(), y.as_f64()) {
        (Some(p), Some(q)) => p.partial_cmp(&q).unwrap_or(std::cmp::Ordering::Equal),
        _ => java_text(x).cmp(&java_text(y)),
    });
    if desc {
        a.reverse();
    }
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(def: Value) -> PipelineError {
        match Pipeline::parse(&Store::scratch(), "p", &def) {
            Ok(_) => panic!("{def} was taken"),
            Err(e) => e,
        }
    }

    #[test]
    fn every_list_names_only_processors_it_has() {
        let e = refused(json!({"phase_results_processors": [{"not-a-processor": {}}]}));
        assert_eq!(e.reason, "Invalid processor type not-a-processor");
        let e = refused(
            json!({"response_processors": [{"filter_query": {"query": {"match_all": {}}}}]}),
        );
        assert_eq!(e.reason, "Invalid processor type filter_query");
    }

    #[test]
    fn what_a_processor_does_not_read_is_named() {
        let e = refused(json!({"response_processors": [
            {"split": {"field": "a", "separator": ",", "tag": "t", "y": 2}}
        ]}));
        assert_eq!(
            e.reason,
            "processor [split:t] doesn't support one or more provided configuration parameters: [y]"
        );
        let e = refused(json!({"request_processors": [], "x": 1}));
        assert_eq!(
            e.reason,
            "pipeline [p] doesn't support one or more provided configuration parameters [x]"
        );
    }

    #[test]
    fn normalization_options_are_checked() {
        let e = refused(json!({"phase_results_processors": [{"normalization-processor": {
            "combination": {"technique": "arithmetic_mean", "parameters": {"weights": [0.5, 0.2]}}
        }}]}));
        assert_eq!(
            e.reason,
            "sum of weights for combination must be equal to 1.0, submitted weights: [0.5, 0.2]"
        );
        let e = refused(json!({"phase_results_processors": [{"normalization-processor": {
            "normalization": {"technique": "z_score"},
            "combination": {"technique": "geometric_mean"}
        }}]}));
        assert!(e.reason.ends_with("Supported techniques are: arithmetic_mean"), "{}", e.reason);
        let taken = Pipeline::parse(
            &Store::scratch(),
            "p",
            &json!({"phase_results_processors": [
                {"score-ranker-processor": {"combination": {"rank_constant": 5}}}
            ]}),
        );
        assert!(taken.is_ok_and(|p| p.scoring().is_some()));
    }
}
