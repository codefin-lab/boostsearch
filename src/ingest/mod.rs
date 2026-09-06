//! Ingest pipelines: what happens to a document on its way in.
//!
//! A pipeline is a list of processors, each reading and changing the
//! document -- `set`, `rename`, `grok`, `date` and the rest OpenSearch
//! names. A processor may run only `if` a script says so, may hand its
//! failure to `on_failure` processors, or may be told to ignore it. The
//! document carries its metadata (`_index`, `_id`, `_routing`) and an
//! `_ingest` section (the timestamp, and what a failure was) beside the
//! source, and a processor may change any of them.

use serde_json::{Map, Value, json};

use crate::store::Store;

pub mod attachment;
pub mod dissect;
pub mod geoip;
pub mod grok;
mod hash;
mod processors;
pub mod user_agent;

/// A document as a pipeline sees it.
#[derive(Clone, Debug, Default)]
pub struct IngestDoc {
    pub index: String,
    pub id: String,
    pub routing: Option<String>,
    pub version: Option<i64>,
    /// the version as the request spelled it, echoed back unchanged
    pub version_raw: Option<Value>,
    pub version_type: Option<String>,
    pub if_seq_no: Option<i64>,
    pub if_primary_term: Option<i64>,
    pub source: Value,
    /// the `_ingest` section: timestamp and the failure being handled
    pub ingest: Map<String, Value>,
}

impl IngestDoc {
    pub fn new(index: &str, id: &str, source: Value) -> IngestDoc {
        let mut ingest = Map::new();
        ingest.insert("timestamp".into(), json!(now_iso()));
        IngestDoc {
            index: index.to_string(),
            id: id.to_string(),
            routing: None,
            version: None,
            version_raw: None,
            version_type: None,
            if_seq_no: None,
            if_primary_term: None,
            source,
            ingest,
        }
    }

    /// The document with its metadata beside the source, the way a script
    /// and a template read it.
    pub fn as_ctx(&self) -> Value {
        let mut m = match &self.source {
            Value::Object(o) => o.clone(),
            other => {
                let mut m = Map::new();
                m.insert("_value".into(), other.clone());
                m
            }
        };
        m.insert("_index".into(), json!(self.index));
        m.insert("_id".into(), json!(self.id));
        if let Some(r) = &self.routing {
            m.insert("_routing".into(), json!(r));
        }
        if let Some(v) = self.version {
            m.insert("_version".into(), json!(v));
        }
        if let Some(v) = &self.version_type {
            m.insert("_version_type".into(), json!(v));
        }
        if let Some(v) = self.if_seq_no {
            m.insert("_if_seq_no".into(), json!(v));
        }
        if let Some(v) = self.if_primary_term {
            m.insert("_if_primary_term".into(), json!(v));
        }
        m.insert("_ingest".into(), Value::Object(self.ingest.clone()));
        Value::Object(m)
    }

    /// Take a context a script changed back into the document.
    pub fn from_ctx(&mut self, ctx: Value) {
        let Value::Object(mut m) = ctx else { return };
        if let Some(Value::String(i)) = m.remove("_index") {
            self.index = i;
        }
        if let Some(v) = m.remove("_id") {
            self.id = match v {
                Value::String(s) => s,
                Value::Null => String::new(),
                other => other.to_string(),
            };
        }
        self.routing = match m.remove("_routing") {
            Some(Value::String(s)) => Some(s),
            Some(Value::Null) | None => None,
            Some(other) => Some(other.to_string()),
        };
        self.version = m.remove("_version").and_then(|v| v.as_i64());
        self.version_type =
            m.remove("_version_type").and_then(|v| v.as_str().map(|s| s.to_string()));
        self.if_seq_no = m.remove("_if_seq_no").and_then(|v| v.as_i64());
        self.if_primary_term = m.remove("_if_primary_term").and_then(|v| v.as_i64());
        if let Some(Value::Object(i)) = m.remove("_ingest") {
            self.ingest = i;
        }
        self.source = Value::Object(m);
    }

    /// The document as the simulate API reports it.
    pub fn to_json(&self) -> Value {
        let mut out = json!({
            "_index": self.index,
            "_id": self.id,
            "_source": self.source,
            "_ingest": Value::Object(self.ingest.clone()),
        });
        if let Some(r) = &self.routing {
            out["_routing"] = json!(r);
        }
        // the numbers among the metadata are reported as text, as
        // OpenSearch writes them
        if let Some(v) = self.version {
            out["_version"] = json!(v.to_string());
        }
        if let Some(v) = &self.version_type {
            out["_version_type"] = json!(v);
        }
        if let Some(v) = self.if_seq_no {
            out["_if_seq_no"] = json!(v.to_string());
        }
        if let Some(v) = self.if_primary_term {
            out["_if_primary_term"] = json!(v.to_string());
        }
        out
    }

    // ---- field access ---------------------------------------------------

    /// Whether a path names a value in the document.
    pub fn has(&self, path: &str) -> bool {
        self.get(path).is_some()
    }

    /// The value at a path: `a.b.c`, `_source.a`, `_ingest.timestamp`, or
    /// one of the metadata fields.
    pub fn get(&self, path: &str) -> Option<Value> {
        let path = path.strip_prefix("_source.").unwrap_or(path);
        match path {
            "_index" => return Some(json!(self.index)),
            "_id" => return Some(json!(self.id)),
            "_routing" => return self.routing.as_ref().map(|r| json!(r)),
            "_version" => return self.version.map(|v| json!(v)),
            "_version_type" => return self.version_type.as_ref().map(|v| json!(v)),
            "_if_seq_no" => return self.if_seq_no.map(|v| json!(v)),
            "_if_primary_term" => return self.if_primary_term.map(|v| json!(v)),
            "_source" => return Some(self.source.clone()),
            "_ingest" => return Some(Value::Object(self.ingest.clone())),
            _ => {}
        }
        if let Some(rest) = path.strip_prefix("_ingest.") {
            return walk(&Value::Object(self.ingest.clone()), rest).cloned();
        }
        walk(&self.source, path).cloned()
    }

    /// Write a value at a path, making the objects along the way.
    pub fn set(&mut self, path: &str, value: Value) -> Result<(), String> {
        let path = path.strip_prefix("_source.").unwrap_or(path);
        match path {
            "_index" => {
                self.index = text_of(&value);
                return Ok(());
            }
            "_id" => {
                self.id = if value.is_null() { String::new() } else { text_of(&value) };
                return Ok(());
            }
            "_routing" => {
                self.routing = if value.is_null() { None } else { Some(text_of(&value)) };
                return Ok(());
            }
            "_version" => {
                self.version = value.as_i64();
                return Ok(());
            }
            "_version_type" => {
                self.version_type = value.as_str().map(|s| s.to_string());
                return Ok(());
            }
            "_if_seq_no" => {
                self.if_seq_no = value.as_i64();
                return Ok(());
            }
            "_if_primary_term" => {
                self.if_primary_term = value.as_i64();
                return Ok(());
            }
            "_source" => {
                self.source = value;
                return Ok(());
            }
            _ => {}
        }
        if let Some(rest) = path.strip_prefix("_ingest.") {
            let mut holder = Value::Object(std::mem::take(&mut self.ingest));
            let out = place(&mut holder, rest, value);
            if let Value::Object(o) = holder {
                self.ingest = o;
            }
            return out;
        }
        if !self.source.is_object() {
            self.source = json!({});
        }
        place(&mut self.source, path, value)
    }

    /// Take a value out, answering what was there.
    pub fn remove(&mut self, path: &str) -> Option<Value> {
        let path = path.strip_prefix("_source.").unwrap_or(path);
        match path {
            "_routing" => return self.routing.take().map(|r| json!(r)),
            "_version" => return self.version.take().map(|v| json!(v)),
            "_version_type" => return self.version_type.take().map(|v| json!(v)),
            "_if_seq_no" => return self.if_seq_no.take().map(|v| json!(v)),
            "_if_primary_term" => return self.if_primary_term.take().map(|v| json!(v)),
            "_index" | "_id" => return None,
            _ => {}
        }
        if let Some(rest) = path.strip_prefix("_ingest.") {
            let mut holder = Value::Object(std::mem::take(&mut self.ingest));
            let out = take(&mut holder, rest);
            if let Value::Object(o) = holder {
                self.ingest = o;
            }
            return out;
        }
        take(&mut self.source, path)
    }

    /// Render a template against the document: `{{field}}`, `{{_ingest.timestamp}}`.
    pub fn render(&self, template: &str) -> String {
        if !template.contains("{{") {
            return template.to_string();
        }
        crate::api::mustache::render(template, &self.as_ctx())
    }

    /// A value that may be a template: rendered where it is text, kept
    /// where it is a number, a list or an object (whose strings are rendered).
    pub fn render_value(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.render(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.render_value(x)).collect()),
            Value::Object(o) => {
                Value::Object(o.iter().map(|(k, x)| (k.clone(), self.render_value(x))).collect())
            }
            other => other.clone(),
        }
    }
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Follow a dotted path through objects (and lists by index).
pub(crate) fn walk<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    // a key holding a dot itself is tried whole before it is split
    if let Value::Object(o) = root
        && let Some(v) = o.get(path)
    {
        return Some(v);
    }
    let mut cur = root;
    let mut rest = path;
    while !rest.is_empty() {
        let (head, tail) = match rest.split_once('.') {
            Some((h, t)) => (h, t),
            None => (rest, ""),
        };
        cur = match cur {
            Value::Object(o) => {
                // the longest dotted key that fits is the one meant
                let mut found = None;
                let mut key = head.to_string();
                let mut remaining = tail;
                loop {
                    if let Some(v) = o.get(&key) {
                        found = Some((v, remaining));
                        break;
                    }
                    match remaining.split_once('.') {
                        Some((h, t)) => {
                            key = format!("{key}.{h}");
                            remaining = t;
                        }
                        None => {
                            if !remaining.is_empty() {
                                key = format!("{key}.{remaining}");
                                remaining = "";
                                if let Some(v) = o.get(&key) {
                                    found = Some((v, remaining));
                                }
                            }
                            break;
                        }
                    }
                }
                let (v, remaining) = found?;
                rest = remaining;
                v
            }
            Value::Array(a) => {
                let i: usize = head.parse().ok()?;
                rest = tail;
                a.get(i)?
            }
            _ => return None,
        };
        if rest.is_empty() {
            return Some(cur);
        }
    }
    Some(cur)
}

fn place(root: &mut Value, path: &str, value: Value) -> Result<(), String> {
    let mut cur = root;
    let mut parts = path.split('.').peekable();
    while let Some(head) = parts.next() {
        let last = parts.peek().is_none();
        match cur {
            Value::Object(o) => {
                if last {
                    o.insert(head.to_string(), value);
                    return Ok(());
                }
                cur = o.entry(head.to_string()).or_insert_with(|| json!({}));
            }
            Value::Array(a) => {
                let i: usize = head.parse().map_err(|_| {
                    format!("[{head}] is not an integer, cannot be used as an index as part of path [{path}]")
                })?;
                if i >= a.len() {
                    return Err(format!(
                        "[{i}] is out of bounds for array with length [{}] as part of path [{path}]",
                        a.len()
                    ));
                }
                if last {
                    a[i] = value;
                    return Ok(());
                }
                cur = &mut a[i];
            }
            other => {
                return Err(format!(
                    "cannot set [{head}] with parent object of type [{}] as part of path [{path}]",
                    type_name(other)
                ));
            }
        }
    }
    Ok(())
}

fn take(root: &mut Value, path: &str) -> Option<Value> {
    if let Value::Object(o) = root
        && o.contains_key(path)
    {
        return o.remove(path);
    }
    let (parent, leaf) = match path.rsplit_once('.') {
        Some((p, l)) => (Some(p), l),
        None => (None, path),
    };
    let holder = match parent {
        Some(p) => walk_mut(root, p)?,
        None => root,
    };
    match holder {
        Value::Object(o) => o.remove(leaf),
        Value::Array(a) => {
            let i: usize = leaf.parse().ok()?;
            (i < a.len()).then(|| a.remove(i))
        }
        _ => None,
    }
}

fn walk_mut<'a>(root: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    let mut cur = root;
    for head in path.split('.') {
        cur = match cur {
            Value::Object(o) => o.get_mut(head)?,
            Value::Array(a) => a.get_mut(head.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

pub(crate) fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "java.lang.Boolean",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "java.lang.Integer"
            } else {
                "java.lang.Double"
            }
        }
        Value::String(_) => "java.lang.String",
        Value::Array(_) => "java.util.ArrayList",
        Value::Object(_) => "java.util.HashMap",
    }
}

pub(crate) fn now_iso() -> String {
    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let ms = now.as_millis() as i64;
    let nanos = now.subsec_nanos() % 1_000_000;
    let base = crate::store::format_millis(ms, "strict_date_optional_time").unwrap_or_default();
    // the timestamp is written to the nanosecond, as Java writes it
    match base.strip_suffix('Z') {
        Some(head) => format!("{head}{nanos:06}Z"),
        None => base,
    }
}

// ---- errors -------------------------------------------------------------

/// What a processor could not do, or a pipeline could not be read.
#[derive(Clone, Debug)]
pub struct IngestError {
    pub kind: String,
    pub reason: String,
    pub processor_type: Option<String>,
    pub processor_tag: Option<String>,
    pub property_name: Option<String>,
    /// the pipeline, processor tag and type a failure carries in `_ingest`
    pub pipeline: Option<String>,
    /// the document as it stood, where a processor found nothing to do
    pub doc_back: Option<IngestDoc>,
    /// a failure inside a pipeline a `pipeline` processor ran, already
    /// reported as that pipeline's own step
    pub nested: bool,
}

impl IngestError {
    pub fn illegal(reason: impl Into<String>) -> IngestError {
        IngestError {
            kind: "illegal_argument_exception".into(),
            reason: reason.into(),
            processor_type: None,
            processor_tag: None,
            property_name: None,
            pipeline: None,
            doc_back: None,
            nested: false,
        }
    }

    pub fn parse(
        reason: impl Into<String>,
        processor_type: Option<&str>,
        tag: Option<&str>,
        property: Option<&str>,
    ) -> IngestError {
        IngestError {
            kind: "parse_exception".into(),
            reason: reason.into(),
            processor_type: processor_type.map(|s| s.to_string()),
            processor_tag: tag.map(|s| s.to_string()),
            property_name: property.map(|s| s.to_string()),
            pipeline: None,
            doc_back: None,
            nested: false,
        }
    }

    pub fn of(kind: &str, reason: impl Into<String>) -> IngestError {
        IngestError { kind: kind.into(), ..IngestError::illegal(reason) }
    }

    /// The error as one cause, the way it sits in `root_cause` or as a
    /// whole error body.
    pub fn cause_json(&self) -> Value {
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
        c
    }

    pub fn body(&self) -> Value {
        let mut top = self.cause_json();
        top["root_cause"] = json!([self.cause_json()]);
        // the order OpenSearch writes: root_cause first
        let mut ordered = Map::new();
        ordered.insert("root_cause".into(), top["root_cause"].clone());
        for (k, v) in top.as_object().into_iter().flatten() {
            if k != "root_cause" {
                ordered.insert(k.clone(), v.clone());
            }
        }
        Value::Object(ordered)
    }

    pub fn status(&self) -> u16 {
        match self.kind.as_str() {
            "resource_not_found_exception" => 404,
            "version_conflict_engine_exception" => 409,
            "fail_processor_exception" | "illegal_state_exception" => 500,
            _ => 400,
        }
    }
}

// ---- pipelines ------------------------------------------------------------

/// A pipeline read from its definition: the processors, and what to do
/// when one of them fails.
#[derive(Clone, Debug)]
pub struct Pipeline {
    pub name: String,
    pub processors: Vec<ProcessorSpec>,
    pub on_failure: Vec<ProcessorSpec>,
}

/// One processor as written: its type, its configuration, and the
/// options every processor has.
#[derive(Clone, Debug)]
pub struct ProcessorSpec {
    pub kind: String,
    pub config: Map<String, Value>,
    pub tag: Option<String>,
    pub description: Option<String>,
    pub condition: Option<Value>,
    pub ignore_failure: bool,
    pub on_failure: Vec<ProcessorSpec>,
}

/// The processor types this engine knows, in the order OpenSearch lists
/// them.
pub const PROCESSOR_TYPES: &[&str] = &[
    "append",
    "bytes",
    "community_id",
    "convert",
    "copy",
    "csv",
    "date",
    "attachment",
    "date_index_name",
    "dissect",
    "dot_expander",
    "drop",
    "fail",
    "fingerprint",
    "foreach",
    "geoip",
    "grok",
    "gsub",
    "html_strip",
    "join",
    "json",
    "kv",
    "lowercase",
    "pipeline",
    "remove",
    "remove_by_pattern",
    "rename",
    "script",
    "set",
    "sort",
    "split",
    "trim",
    "uppercase",
    "urldecode",
    "user_agent",
];

impl Pipeline {
    /// Read a pipeline definition, refusing what cannot be run.
    pub fn parse(name: &str, def: &Value) -> Result<Pipeline, IngestError> {
        let Some(o) = def.as_object() else {
            return Err(IngestError::parse(
                "[pipeline] pipeline definition is not an object",
                None,
                None,
                None,
            ));
        };
        let processors = match o.get("processors") {
            Some(Value::Array(list)) => parse_processors(list)?,
            Some(_) => {
                return Err(IngestError::parse(
                    "[processors] property isn't a list, but of type [java.lang.String]",
                    None,
                    None,
                    Some("processors"),
                ));
            }
            None => {
                return Err(IngestError::parse(
                    "[processors] required property is missing",
                    None,
                    None,
                    Some("processors"),
                ));
            }
        };
        let on_failure = match o.get("on_failure") {
            Some(Value::Array(list)) => {
                if list.is_empty() {
                    return Err(IngestError::parse(
                        format!("pipeline [{name}] cannot have an empty on_failure option defined"),
                        None,
                        None,
                        Some("on_failure"),
                    ));
                }
                parse_processors(list)?
            }
            Some(_) => {
                return Err(IngestError::parse(
                    "[on_failure] property isn't a list, but of type [java.lang.String]",
                    None,
                    None,
                    Some("on_failure"),
                ));
            }
            None => Vec::new(),
        };
        Ok(Pipeline { name: name.to_string(), processors, on_failure })
    }
}

/// Every template a processor's configuration holds, checked.
fn templates_within(cfg: &serde_json::Map<String, Value>) -> Result<(), String> {
    fn walk(v: &Value) -> Result<(), String> {
        match v {
            Value::String(text) => crate::api::mustache::check(text),
            Value::Array(items) => items.iter().try_for_each(walk),
            Value::Object(o) => o.values().try_for_each(walk),
            _ => Ok(()),
        }
    }
    cfg.values().try_for_each(walk)
}

fn parse_processors(list: &[Value]) -> Result<Vec<ProcessorSpec>, IngestError> {
    let mut out = Vec::new();
    for item in list {
        let Some(o) = item.as_object() else {
            return Err(IngestError::parse(
                "processor definition is not an object",
                None,
                None,
                None,
            ));
        };
        if o.len() != 1 {
            return Err(IngestError::parse(
                "[processors] each processor must have exactly one key",
                None,
                None,
                None,
            ));
        }
        let (kind, cfg) = o.iter().next().unwrap();
        let Some(cfg) = cfg.as_object() else {
            return Err(IngestError::parse(
                format!("[{kind}] processor configuration is not an object"),
                Some(kind),
                None,
                None,
            ));
        };
        // a value in a processor's configuration may be a template, and one
        // that cannot be rendered is refused now rather than at the first
        // document that goes through it
        if let Err(reason) = templates_within(cfg) {
            return Err(IngestError {
                processor_type: Some(kind.to_string()),
                ..IngestError::of("script_exception", reason)
            });
        }
        let mut config = cfg.clone();
        let tag = config.remove("tag").and_then(|v| v.as_str().map(|s| s.to_string()));
        let description =
            config.remove("description").and_then(|v| v.as_str().map(|s| s.to_string()));
        let condition = config.remove("if");
        let ignore_failure =
            config.remove("ignore_failure").and_then(|v| v.as_bool()).unwrap_or(false);
        let on_failure = match config.remove("on_failure") {
            Some(Value::Array(list)) => {
                if list.is_empty() {
                    return Err(IngestError::parse(
                        "[on_failure] processors list cannot be empty",
                        Some(kind),
                        tag.as_deref(),
                        Some("on_failure"),
                    ));
                }
                parse_processors(&list)?
            }
            Some(_) => {
                return Err(IngestError::parse(
                    "[on_failure] property isn't a list, but of type [java.lang.String]",
                    Some(kind),
                    tag.as_deref(),
                    Some("on_failure"),
                ));
            }
            None => Vec::new(),
        };
        if !PROCESSOR_TYPES.contains(&kind.as_str()) {
            return Err(IngestError::parse(
                format!("No processor type exists with name [{kind}]"),
                Some(kind),
                tag.as_deref(),
                None,
            ));
        }
        let spec = ProcessorSpec {
            kind: kind.clone(),
            config,
            tag,
            description,
            condition,
            ignore_failure,
            on_failure,
        };
        processors::check(&spec)?;
        out.push(spec);
    }
    Ok(out)
}

/// One processor's outcome, for the simulate API's verbose answer.
#[derive(Clone, Debug)]
pub struct StepResult {
    pub processor_type: String,
    pub tag: Option<String>,
    pub description: Option<String>,
    pub status: &'static str,
    pub doc: Option<IngestDoc>,
    pub error: Option<IngestError>,
    pub condition_met: Option<bool>,
    pub condition_text: Option<String>,
}

/// What a pipeline run leaves behind.
pub struct Outcome {
    /// the document, or nothing where a `drop` took it
    pub doc: Option<IngestDoc>,
    pub steps: Vec<StepResult>,
}

/// Run a pipeline over a document.
pub fn run_pipeline(
    store: &Store,
    pipeline: &Pipeline,
    doc: IngestDoc,
    steps: &mut Vec<StepResult>,
    depth: &mut Vec<String>,
) -> Result<Option<IngestDoc>, IngestError> {
    if depth.contains(&pipeline.name) {
        return Err(IngestError::of(
            "illegal_state_exception",
            format!("Cycle detected for pipeline: {}", pipeline.name),
        ));
    }
    depth.push(pipeline.name.clone());
    let out = run_processors(store, &pipeline.name, &pipeline.processors, doc, steps, depth);
    let out = match out {
        Err(e) if !pipeline.on_failure.is_empty() => {
            // the pipeline's own on_failure handles what its processors
            // could not; the document goes on from where it was
            let doc = e.1;
            let mut doc = doc;
            note_failure(&mut doc, &e.0);
            let handled =
                run_processors(store, &pipeline.name, &pipeline.on_failure, doc, steps, depth);
            match handled {
                Ok(d) => Ok(d.map(|mut d| {
                    clear_failure(&mut d);
                    d
                })),
                Err(e) => Err(e.0),
            }
        }
        Err(e) => Err(e.0),
        Ok(d) => Ok(d),
    };
    depth.pop();
    out
}

fn note_failure(doc: &mut IngestDoc, e: &IngestError) {
    doc.ingest.insert("on_failure_message".into(), json!(e.reason));
    doc.ingest.insert("on_failure_processor_type".into(), json!(e.processor_type));
    doc.ingest.insert("on_failure_processor_tag".into(), json!(e.processor_tag));
    if let Some(p) = &e.pipeline {
        doc.ingest.insert("on_failure_pipeline".into(), json!(p));
    }
}

fn clear_failure(doc: &mut IngestDoc) {
    for k in [
        "on_failure_message",
        "on_failure_processor_type",
        "on_failure_processor_tag",
        "on_failure_pipeline",
    ] {
        doc.ingest.remove(k);
    }
}

/// Run a list of processors; a failure comes back with the document as
/// it stood when the processor failed, for whoever handles it.
fn run_processors(
    store: &Store,
    pipeline: &str,
    list: &[ProcessorSpec],
    doc: IngestDoc,
    steps: &mut Vec<StepResult>,
    depth: &mut Vec<String>,
) -> Result<Option<IngestDoc>, (IngestError, IngestDoc)> {
    let mut doc = doc;
    for spec in list {
        // a condition says whether this processor runs at all
        if let Some(cond) = &spec.condition {
            let met = match processors::condition_holds(store, cond, &doc) {
                Ok(m) => m,
                Err(e) => {
                    let e = e.with_processor(spec, pipeline);
                    if spec.ignore_failure {
                        steps.push(StepResult {
                            processor_type: spec.kind.clone(),
                            tag: spec.tag.clone(),
                            description: spec.description.clone(),
                            status: "error_ignored",
                            doc: Some(doc.clone()),
                            error: Some(e),
                            condition_met: None,
                            condition_text: None,
                        });
                        continue;
                    }
                    steps.push(StepResult {
                        processor_type: spec.kind.clone(),
                        tag: spec.tag.clone(),
                        description: spec.description.clone(),
                        status: "error",
                        doc: None,
                        error: Some(e.clone()),
                        condition_met: None,
                        condition_text: None,
                    });
                    return Err((e, doc));
                }
            };
            if !met {
                steps.push(StepResult {
                    processor_type: spec.kind.clone(),
                    tag: spec.tag.clone(),
                    description: spec.description.clone(),
                    status: "skipped",
                    doc: Some(doc.clone()),
                    error: None,
                    condition_met: Some(false),
                    condition_text: spec.condition.as_ref().map(condition_source),
                });
                continue;
            }
        }
        let before = doc.clone();
        match processors::run(store, spec, doc, steps, depth) {
            Ok(Some(d)) if spec.kind == "pipeline" => {
                doc = d;
            }
            Ok(Some(d)) => {
                steps.push(StepResult {
                    processor_type: spec.kind.clone(),
                    tag: spec.tag.clone(),
                    description: spec.description.clone(),
                    status: "success",
                    doc: Some(d.clone()),
                    error: None,
                    condition_met: spec.condition.as_ref().map(|_| true),
                    condition_text: spec.condition.as_ref().map(condition_source),
                });
                doc = d;
            }
            Ok(None) => {
                steps.push(StepResult {
                    processor_type: spec.kind.clone(),
                    tag: spec.tag.clone(),
                    description: spec.description.clone(),
                    status: "dropped",
                    doc: None,
                    error: None,
                    condition_met: spec.condition.as_ref().map(|_| true),
                    condition_text: spec.condition.as_ref().map(condition_source),
                });
                return Ok(None);
            }
            Err(e) if e.nested => {
                // the failing step inside the named pipeline is listed already
                let mut e = e.with_processor(spec, pipeline);
                e.nested = false;
                if spec.ignore_failure {
                    doc = before;
                    continue;
                }
                if spec.on_failure.is_empty() {
                    return Err((e, before));
                }
                let mut handed = before;
                note_failure(&mut handed, &e);
                match run_processors(store, pipeline, &spec.on_failure, handed, steps, depth) {
                    Ok(Some(mut d)) => {
                        clear_failure(&mut d);
                        doc = d;
                    }
                    Ok(None) => return Ok(None),
                    Err(inner) => return Err(inner),
                }
            }
            Err(e) => {
                let e = e.with_processor(spec, pipeline);
                if spec.ignore_failure {
                    steps.push(StepResult {
                        processor_type: spec.kind.clone(),
                        tag: spec.tag.clone(),
                        description: spec.description.clone(),
                        status: "error_ignored",
                        doc: Some(before.clone()),
                        error: Some(e),
                        condition_met: spec.condition.as_ref().map(|_| true),
                        condition_text: spec.condition.as_ref().map(condition_source),
                    });
                    doc = before;
                    continue;
                }
                steps.push(StepResult {
                    processor_type: spec.kind.clone(),
                    tag: spec.tag.clone(),
                    description: spec.description.clone(),
                    status: "error",
                    doc: None,
                    error: Some(e.clone()),
                    condition_met: spec.condition.as_ref().map(|_| true),
                    condition_text: spec.condition.as_ref().map(condition_source),
                });
                if spec.on_failure.is_empty() {
                    return Err((e, before));
                }
                let mut handed = before;
                note_failure(&mut handed, &e);
                match run_processors(store, pipeline, &spec.on_failure, handed, steps, depth) {
                    Ok(Some(mut d)) => {
                        clear_failure(&mut d);
                        doc = d;
                    }
                    Ok(None) => return Ok(None),
                    Err(inner) => return Err(inner),
                }
            }
        }
    }
    Ok(Some(doc))
}

impl IngestError {
    fn with_processor(mut self, spec: &ProcessorSpec, pipeline: &str) -> IngestError {
        if self.processor_type.is_none() {
            self.processor_type = Some(spec.kind.clone());
        }
        if self.processor_tag.is_none() {
            self.processor_tag = spec.tag.clone();
        }
        if self.pipeline.is_none() {
            self.pipeline = Some(pipeline.to_string());
        }
        self
    }
}

/// The pipeline stored under a name, read for running.
pub fn stored_pipeline(store: &Store, name: &str) -> Option<Result<Pipeline, IngestError>> {
    let def = store.pipelines("ingest").remove(name)?;
    Some(Pipeline::parse(name, &def))
}

/// A size written with a unit, in bytes: what the `bytes` processor and
/// `Processors.bytes` read.
pub fn bytes_of_text(s: &str) -> Result<i64, String> {
    processors::bytes_of(s).map_err(|e| e.reason)
}

/// The text of a condition, as written.
fn condition_source(cond: &Value) -> String {
    match cond {
        Value::String(s) => s.clone(),
        other => other.get("source").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    }
}
