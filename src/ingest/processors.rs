//! The processors themselves: what each one reads from its configuration,
//! and what it does to the document.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::{IngestDoc, IngestError, ProcessorSpec, StepResult, type_name};
use crate::store::Store;

// ---- configuration reading -------------------------------------------------

struct Cfg<'a> {
    spec: &'a ProcessorSpec,
}

impl<'a> Cfg<'a> {
    fn missing(&self, key: &str) -> IngestError {
        IngestError::parse(
            format!("[{key}] required property is missing"),
            Some(&self.spec.kind),
            self.spec.tag.as_deref(),
            Some(key),
        )
    }

    fn wrong(&self, key: &str, reason: String) -> IngestError {
        IngestError::parse(reason, Some(&self.spec.kind), self.spec.tag.as_deref(), Some(key))
    }

    fn get(&self, key: &str) -> Option<&'a Value> {
        self.spec.config.get(key)
    }

    fn str_req(&self, key: &str) -> Result<String, IngestError> {
        match self.get(key) {
            None | Some(Value::Null) => Err(self.missing(key)),
            Some(Value::String(s)) => Ok(s.clone()),
            Some(other) => Err(self.wrong(
                key,
                format!("[{key}] property isn't a string, but of type [{}]", type_name(other)),
            )),
        }
    }

    fn str_opt(&self, key: &str) -> Result<Option<String>, IngestError> {
        match self.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(Value::Number(n)) => Ok(Some(n.to_string())),
            Some(Value::Bool(b)) => Ok(Some(b.to_string())),
            Some(other) => Err(self.wrong(
                key,
                format!("[{key}] property isn't a string, but of type [{}]", type_name(other)),
            )),
        }
    }

    fn bool_opt(&self, key: &str, default: bool) -> Result<bool, IngestError> {
        match self.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(b)) => Ok(*b),
            Some(Value::String(s)) if s == "true" || s == "false" => Ok(s == "true"),
            Some(other) => Err(self.wrong(
                key,
                format!("[{key}] property isn't a boolean, but of type [{}]", type_name(other)),
            )),
        }
    }

    fn int_opt(&self, key: &str) -> Result<Option<i64>, IngestError> {
        match self.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(n)) => Ok(n.as_i64()),
            Some(Value::String(s)) => s.parse::<i64>().map(Some).map_err(|_| {
                self.wrong(key, format!("[{key}] property cannot be converted to an int [{s}]"))
            }),
            Some(other) => Err(self.wrong(
                key,
                format!("[{key}] property isn't an int, but of type [{}]", type_name(other)),
            )),
        }
    }

    fn list_opt(&self, key: &str) -> Result<Option<Vec<Value>>, IngestError> {
        match self.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Array(a)) => Ok(Some(a.clone())),
            Some(other) => Err(self.wrong(
                key,
                format!("[{key}] property isn't a list, but of type [{}]", type_name(other)),
            )),
        }
    }

    fn strings_opt(&self, key: &str) -> Result<Option<Vec<String>>, IngestError> {
        Ok(self.list_opt(key)?.map(|list| {
            list.iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect()
        }))
    }

    fn strings_req(&self, key: &str) -> Result<Vec<String>, IngestError> {
        self.strings_opt(key)?.ok_or_else(|| self.missing(key))
    }

    fn map_opt(&self, key: &str) -> Result<Option<Map<String, Value>>, IngestError> {
        match self.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Object(o)) => Ok(Some(o.clone())),
            Some(other) => Err(self.wrong(
                key,
                format!("[{key}] property isn't a map, but of type [{}]", type_name(other)),
            )),
        }
    }
}

/// Refuse a processor whose configuration cannot be run, as the pipeline
/// is put rather than as the first document arrives.
pub(crate) fn check(spec: &ProcessorSpec) -> Result<(), IngestError> {
    let c = Cfg { spec };
    let field_required = |c: &Cfg| c.str_req("field").map(|_| ());
    match spec.kind.as_str() {
        "set" => {
            c.str_req("field")?;
            if c.get("value").is_none() && c.get("copy_from").is_none() {
                return Err(c.missing("value"));
            }
            if c.get("value").is_some() && c.get("copy_from").is_some() {
                return Err(c.wrong(
                    "copy_from",
                    "[copy_from] cannot set both `copy_from` and `value` in the same processor"
                        .into(),
                ));
            }
        }
        "append" => {
            c.str_req("field")?;
            if c.get("value").is_none() {
                return Err(c.missing("value"));
            }
        }
        "rename" => {
            c.str_req("field")?;
            c.str_req("target_field")?;
        }
        "remove" => {
            if c.get("field").is_none() && c.get("exclude_field").is_none() {
                return Err(c.missing("field"));
            }
            if c.get("field").is_some() && c.get("exclude_field").is_some() {
                return Err(
                    c.wrong("field", "[field] ether field or exclude_field must be set".into())
                );
            }
        }
        "copy" => {
            c.str_req("source_field")?;
            c.str_req("target_field")?;
        }
        "remove_by_pattern" => {
            if c.get("field_pattern").is_none() && c.get("exclude_field_pattern").is_none() {
                return Err(c.wrong(
                    "field_pattern",
                    "[field_pattern] either field_pattern or exclude_field_pattern must be set"
                        .into(),
                ));
            }
            if c.get("field_pattern").is_some() && c.get("exclude_field_pattern").is_some() {
                return Err(c.wrong(
                    "field_pattern",
                    "[field_pattern] either field_pattern or exclude_field_pattern must be set"
                        .into(),
                ));
            }
            for key in ["field_pattern", "exclude_field_pattern"] {
                let pats: Vec<String> = match c.get(key) {
                    Some(Value::String(s)) => vec![s.clone()],
                    Some(Value::Array(a)) => {
                        a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()
                    }
                    _ => Vec::new(),
                };
                for p in pats {
                    if p.starts_with('_') {
                        return Err(c.wrong(
                            key,
                            format!(
                                "[{key}] Validation Failed: 1: {key} [{p}] must not start with '_';"
                            ),
                        ));
                    }
                    if p.contains('#') || p.contains(':') || p.contains(',') {
                        return Err(c.wrong(key, format!("[{key}] Validation Failed: 1: {key} [{p}] must not contain the following characters [ , \", \\, <, |, ,, >, /, ?, #, :];")));
                    }
                }
            }
        }
        "lowercase" | "uppercase" | "trim" | "bytes" | "urldecode" | "html_strip" | "json"
        | "sort" | "dot_expander" | "kv" | "csv" | "date" | "date_index_name" | "grok"
        | "dissect" | "gsub" | "split" | "join" | "convert" | "foreach" | "user_agent"
        | "geoip" => {
            field_required(&c)?;
            match spec.kind.as_str() {
                "kv" => {
                    c.str_req("field_split")?;
                    c.str_req("value_split")?;
                }
                "csv" => {
                    c.strings_req("target_fields")?;
                }
                "date" => {
                    c.strings_req("formats")?;
                }
                "date_index_name" => {
                    c.str_req("date_rounding")?;
                }
                "grok" => {
                    let patterns = c.strings_req("patterns")?;
                    if patterns.is_empty() {
                        return Err(c.wrong(
                            "patterns",
                            "[patterns] List of patterns must not be empty".into(),
                        ));
                    }
                    let defs: HashMap<String, String> = c
                        .map_opt("pattern_definitions")?
                        .map(|m| {
                            m.iter()
                                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                                .collect()
                        })
                        .unwrap_or_default();
                    for p in &patterns {
                        super::grok::Grok::compile(p, &defs)
                            .map_err(|e| c.wrong("patterns", e.reason))?;
                    }
                }
                "dissect" => {
                    let pattern = c.str_req("pattern")?;
                    let sep = c.str_opt("append_separator")?.unwrap_or_default();
                    super::dissect::Dissect::compile(&pattern, &sep)
                        .map_err(|e| c.wrong("pattern", e.reason))?;
                }
                "gsub" => {
                    let pattern = c.str_req("pattern")?;
                    c.str_req("replacement")?;
                    regex::Regex::new(&pattern).map_err(|e| {
                        c.wrong("pattern", format!("[pattern] Invalid regex pattern. {e}"))
                    })?;
                }
                "split" => {
                    c.str_req("separator")?;
                }
                "join" => {
                    c.str_req("separator")?;
                }
                "convert" => {
                    let kind = c.str_req("type")?;
                    if !["integer", "long", "float", "double", "string", "boolean", "ip", "auto"]
                        .contains(&kind.to_lowercase().as_str())
                    {
                        return Err(c.wrong(
                            "type",
                            format!("[type] type [{kind}] not supported, cannot convert field."),
                        ));
                    }
                }
                "foreach" => {
                    let Some(Value::Object(p)) = c.get("processor") else {
                        return Err(c.missing("processor"));
                    };
                    if p.len() != 1 {
                        return Err(c.wrong(
                            "processor",
                            "[processor] Must specify exactly one processor type".into(),
                        ));
                    }
                    super::parse_processors(&[Value::Object(p.clone())])?;
                }
                "json" => {
                    if c.get("add_to_root").is_some()
                        && c.get("target_field").is_some()
                        && c.bool_opt("add_to_root", false)?
                    {
                        return Err(c.wrong("add_to_root", "[add_to_root] Cannot set a target field while also setting `add_to_root` to true".into()));
                    }
                    if let Some(s) = c.str_opt("add_to_root_conflict_strategy")?
                        && !["replace", "merge"].contains(&s.as_str())
                    {
                        return Err(c.wrong("add_to_root_conflict_strategy", format!("[add_to_root_conflict_strategy] conflict strategy [{s}] not supported, cannot convert field.")));
                    }
                }
                "user_agent" => {
                    if let Some(f) = c.str_opt("regex_file")? {
                        super::user_agent::parser(Some(&f))
                            .map_err(|e| c.wrong("regex_file", e.reason))?;
                    }
                    if let Some(props) = c.strings_opt("properties")? {
                        for p in props {
                            if !["name", "os", "device", "original", "version"]
                                .contains(&p.as_str())
                            {
                                return Err(c.wrong("properties", format!("[properties] illegal property value [{p}]. valid values are [NAME, OS, DEVICE, ORIGINAL, VERSION]")));
                            }
                        }
                    }
                }
                "sort" => {
                    if let Some(o) = c.str_opt("order")?
                        && o != "asc"
                        && o != "desc"
                    {
                        return Err(c.wrong("order", format!("[order] Sort direction [{o}] not recognized. Valid values are: [asc, desc]")));
                    }
                }
                _ => {}
            }
        }
        "fail" => {
            c.str_req("message")?;
        }
        "pipeline" => {
            c.str_req("name")?;
        }
        "script" => {
            if c.get("source").is_none() && c.get("id").is_none() && c.get("inline").is_none() {
                return Err(c.wrong(
                    "source",
                    "[source] Need [source] or [id] parameter to refer to scripts".into(),
                ));
            }
            if c.get("source").is_some() && c.get("id").is_some() {
                return Err(c.wrong(
                    "source",
                    "[source] Cannot specify both [source] and [id] parameters".into(),
                ));
            }
            if let Some(Value::String(src)) = c.get("source") {
                let _ = crate::painless::Script::compile(src).map_err(|e| IngestError {
                    kind: "script_exception".into(),
                    reason: e.kind.to_string(),
                    processor_type: Some("script".into()),
                    processor_tag: spec.tag.clone(),
                    property_name: None,
                    pipeline: None,
                    doc_back: None,
                    nested: false,
                })?;
            }
        }
        "fingerprint" => {
            let method = c.str_opt("hash_method")?.unwrap_or_else(|| "SHA-1@2.16.0".into());
            super::hash::digest(&method, b"")
                .map_err(|e| c.wrong("hash_method", format!("[hash_method] {}", e.reason)))?;
            if c.get("fields").is_some() && c.get("exclude_fields").is_some() {
                return Err(c.wrong(
                    "fields",
                    "[fields] either fields or exclude_fields can be set".into(),
                ));
            }
            for key in ["fields", "exclude_fields"] {
                if let Some(list) = c.strings_opt(key)? {
                    if list.iter().any(|f| f.trim().is_empty() || f == "null") {
                        return Err(
                            c.wrong(key, format!("[{key}] field name cannot be null nor empty"))
                        );
                    }
                }
            }
        }
        "community_id" => {
            c.str_req("source_ip_field")?;
            c.str_req("destination_ip_field")?;
            if let Some(seed) = c.int_opt("seed")?
                && !(0..=65535).contains(&seed)
            {
                return Err(c.wrong(
                    "seed",
                    format!("[seed] seed [{seed}] must be a value between 0 and 65535"),
                ));
            }
        }
        "drop" => {}
        _ => {}
    }
    // a template that never closes is a mistake worth naming now
    for (k, v) in &spec.config {
        if let Value::String(s) = v
            && let Some(name) = crate::api::mustache::unclosed(s)
        {
            return Err(c.wrong(k, format!("[{k}] Mustache tag [{name}] was not closed")));
        }
    }
    Ok(())
}

/// Whether a processor's `if` holds for a document.
pub(crate) fn condition_holds(
    store: &Store,
    cond: &Value,
    doc: &IngestDoc,
) -> Result<bool, IngestError> {
    let spec = match cond {
        Value::String(s) => json!({"source": s}),
        other => other.clone(),
    };
    let compiled = crate::painless::contexts::Compiled::of(&spec, &|id| store.stored_script(id))
        .map_err(|e| IngestError::of("script_exception", e.kind))?;
    let ctx = crate::painless::Value::from_json(&doc.as_ctx());
    let mut runner = crate::painless::contexts::Runner::new(&compiled.params).with_ctx(ctx);
    let out =
        runner.run(&compiled.script).map_err(|e| IngestError::of("script_exception", e.message))?;
    Ok(out.truthy().unwrap_or(false))
}

fn no_field(field: &str) -> IngestError {
    IngestError::illegal(format!("field [{field}] doesn't exist"))
}

fn field_null(field: &str) -> IngestError {
    IngestError::illegal(format!("field [{field}] is null, cannot be processed"))
}

/// Read the field a processor names, rendered as a template first. An
/// empty name is a missing field where the processor was told to ignore
/// those, and a fault otherwise.
fn field_of(doc: &IngestDoc, c: &Cfg, key: &str) -> Result<String, IngestError> {
    let name = doc.render(&c.str_req(key)?);
    if name.trim().is_empty() && key == "field" && c.bool_opt("ignore_missing", false)? {
        return Err(IngestError::of("__missing__", ""));
    }
    if name.trim().is_empty() {
        let what = match key {
            "source_field" => "source field path",
            "target_field" => "target field path",
            _ => "field path",
        };
        return Err(IngestError::illegal(format!("{what} cannot be null nor empty")));
    }
    Ok(name)
}

/// An optional field name, rendered.
fn field_opt(doc: &IngestDoc, c: &Cfg, key: &str) -> Result<Option<String>, IngestError> {
    Ok(c.str_opt(key)?.map(|t| doc.render(&t)))
}

fn string_at(
    doc: &IngestDoc,
    field: &str,
    ignore_missing: bool,
) -> Result<Option<String>, IngestError> {
    match doc.get(field) {
        None => {
            if ignore_missing {
                Ok(None)
            } else {
                Err(no_field(field))
            }
        }
        Some(Value::Null) => {
            if ignore_missing {
                Ok(None)
            } else {
                Err(field_null(field))
            }
        }
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(IngestError::illegal(format!(
            "field [{field}] of type [{}] cannot be cast to [java.lang.String]",
            type_name(&other)
        ))),
    }
}

/// Run one processor over the document.
pub(crate) fn run(
    store: &Store,
    spec: &ProcessorSpec,
    mut doc: IngestDoc,
    steps: &mut Vec<StepResult>,
    depth: &mut Vec<String>,
) -> Result<Option<IngestDoc>, IngestError> {
    let c = Cfg { spec };
    let ignore_missing = c.bool_opt("ignore_missing", false)?;
    let out = run_inner(store, spec, &c, ignore_missing, doc, steps, depth);
    match out {
        Err(e) if e.kind == "__missing__" => Ok(Some(e.doc_back.unwrap_or_default())),
        other => other,
    }
}

fn run_inner(
    store: &Store,
    spec: &ProcessorSpec,
    c: &Cfg,
    ignore_missing: bool,
    mut doc: IngestDoc,
    steps: &mut Vec<StepResult>,
    depth: &mut Vec<String>,
) -> Result<Option<IngestDoc>, IngestError> {
    let c = Cfg { spec: c.spec };
    let c = &c;
    // a processor that finds nothing to do hands the document back untouched
    let keep = doc.clone();
    let out = run_body(store, spec, c, ignore_missing, doc, steps, depth);
    match out {
        Err(mut e) if e.kind == "__missing__" => {
            e.doc_back = Some(keep);
            Err(e)
        }
        other => other,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_body(
    store: &Store,
    spec: &ProcessorSpec,
    c: &Cfg,
    ignore_missing: bool,
    mut doc: IngestDoc,
    steps: &mut Vec<StepResult>,
    depth: &mut Vec<String>,
) -> Result<Option<IngestDoc>, IngestError> {
    let _ = &mut doc;
    match spec.kind.as_str() {
        "set" => {
            let field = field_of(&doc, c, "field")?;
            let override_ = c.bool_opt("override", true)?;
            let ignore_empty = c.bool_opt("ignore_empty_value", false)?;
            if !override_ && doc.get(&field).map(|v| !v.is_null()).unwrap_or(false) {
                return Ok(Some(doc));
            }
            let value = match c.get("copy_from") {
                Some(Value::String(from)) => match doc.get(&doc.render(from)) {
                    Some(v) => v,
                    None => {
                        if ignore_empty {
                            return Ok(Some(doc));
                        }
                        return Err(no_field(from));
                    }
                },
                _ => doc.render_value(c.get("value").unwrap_or(&Value::Null)),
            };
            if ignore_empty
                && (value.is_null() || value.as_str().map(|s| s.is_empty()).unwrap_or(false))
            {
                return Ok(Some(doc));
            }
            doc.set(&field, value).map_err(IngestError::illegal)?;
        }
        "append" => {
            let field = field_of(&doc, c, "field")?;
            let allow_dupes = c.bool_opt("allow_duplicates", true)?;
            let value = doc.render_value(c.get("value").unwrap_or(&Value::Null));
            let more: Vec<Value> = match value {
                Value::Array(a) => a,
                other => vec![other],
            };
            let mut list = match doc.get(&field) {
                Some(Value::Array(a)) => a,
                Some(Value::Null) | None => Vec::new(),
                Some(other) => vec![other],
            };
            for v in more {
                if allow_dupes || !list.contains(&v) {
                    list.push(v);
                }
            }
            doc.set(&field, Value::Array(list)).map_err(IngestError::illegal)?;
        }
        "rename" => {
            let field = field_of(&doc, c, "field")?;
            let target = field_of(&doc, c, "target_field")?;
            let override_ = c.bool_opt("override_target", false)?;
            if !doc.has(&field) {
                if ignore_missing {
                    return Ok(Some(doc));
                }
                return Err(no_field(&field));
            }
            if doc.has(&target) && !override_ {
                return Err(IngestError::illegal(format!("field [{target}] already exists")));
            }
            let value = doc.remove(&field).unwrap_or(Value::Null);
            doc.set(&target, value).map_err(IngestError::illegal)?;
        }
        "remove" => {
            // `exclude_field` keeps only the fields it names
            if c.get("field").is_none() {
                let keep: Vec<String> = match c.get("exclude_field") {
                    Some(Value::String(s)) => vec![doc.render(s)],
                    Some(Value::Array(a)) => {
                        a.iter().map(|v| doc.render(&super::hash::java_text(v))).collect()
                    }
                    _ => Vec::new(),
                };
                if let Value::Object(o) = &mut doc.source {
                    let names: Vec<String> = o.keys().cloned().collect();
                    for n in names {
                        if !keep.contains(&n) {
                            o.remove(&n);
                        }
                    }
                }
                return Ok(Some(doc));
            }
            let fields: Vec<String> = match c.get("field") {
                Some(Value::Array(a)) => {
                    a.iter().map(|v| doc.render(&super::hash::java_text(v))).collect()
                }
                Some(v) => vec![doc.render(&super::hash::java_text(v))],
                None => Vec::new(),
            };
            for field in fields {
                if field.trim().is_empty() {
                    if ignore_missing {
                        continue;
                    }
                    return Err(IngestError::illegal("field path cannot be null nor empty"));
                }
                if field == "_id"
                    && let (Some(v), Some(t)) = (doc.version, doc.version_type.as_deref())
                    && t.starts_with("external")
                {
                    return Err(IngestError::illegal(format!(
                        "cannot remove metadata field [_id] when specifying external version for the document, version: {v}, version_type: {t}"
                    )));
                }
                // the id may go -- a fresh one is made up -- but the index and
                // the version fields may not
                if field == "_id" {
                    doc.id = String::new();
                    continue;
                }
                if matches!(
                    field.as_str(),
                    "_index" | "_version" | "_version_type" | "_if_seq_no" | "_if_primary_term"
                ) {
                    return Err(IngestError::illegal(format!(
                        "cannot remove metadata field [{field}]"
                    )));
                }
                if !doc.has(&field) {
                    if ignore_missing {
                        continue;
                    }
                    return Err(no_field(&field));
                }
                doc.remove(&field);
            }
        }
        "remove_by_pattern" => {
            let listed = |key: &str| -> Vec<String> {
                match c.get(key) {
                    Some(Value::String(s)) => vec![s.clone()],
                    Some(Value::Array(a)) => {
                        a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()
                    }
                    _ => Vec::new(),
                }
            };
            let keep = listed("exclude_field_pattern");
            let drop = listed("field_pattern");
            if let Value::Object(o) = &mut doc.source {
                let names: Vec<String> = o.keys().cloned().collect();
                for name in names {
                    let matched_drop = drop.iter().any(|p| crate::store::glob_match(p, &name));
                    let matched_keep = keep.iter().any(|p| crate::store::glob_match(p, &name));
                    if (!drop.is_empty() && matched_drop) || (!keep.is_empty() && !matched_keep) {
                        o.remove(&name);
                    }
                }
            }
        }
        "copy" => {
            let source = field_of(&doc, c, "source_field")?;
            let target = field_of(&doc, c, "target_field")?;
            if source == target {
                return Err(IngestError::illegal(
                    "source field path and target field path cannot be same",
                ));
            }
            let override_ = c.bool_opt("override_target", false)?;
            let remove_source = c.bool_opt("remove_source", false)?;
            let Some(value) = doc.get(&source) else {
                if ignore_missing {
                    return Ok(Some(doc));
                }
                return Err(IngestError::illegal(format!("source field [{source}] doesn't exist")));
            };
            if doc.has(&target) && !override_ {
                return Err(IngestError::illegal(format!(
                    "target field [{target}] already exists"
                )));
            }
            doc.set(&target, value).map_err(IngestError::illegal)?;
            if remove_source {
                doc.remove(&source);
            }
        }
        "lowercase" | "uppercase" | "trim" | "urldecode" | "html_strip" | "bytes" => {
            let field = field_of(&doc, c, "field")?;
            let target = field_opt(&doc, c, "target_field")?.unwrap_or_else(|| field.clone());
            let Some(text) = string_at(&doc, &field, ignore_missing)? else { return Ok(Some(doc)) };
            let apply = |s: &str| -> Result<Value, IngestError> {
                Ok(match spec.kind.as_str() {
                    "lowercase" => json!(s.to_lowercase()),
                    "uppercase" => json!(s.to_uppercase()),
                    "trim" => json!(s.trim()),
                    "urldecode" => json!(
                        percent_encoding::percent_decode_str(&s.replace('+', " "))
                            .decode_utf8_lossy()
                            .to_string()
                    ),
                    "html_strip" => json!(strip_html(s)),
                    _ => json!(bytes_of(s)?),
                })
            };
            let value = apply(&text)?;
            doc.set(&target, value).map_err(IngestError::illegal)?;
        }
        "split" => {
            let field = field_of(&doc, c, "field")?;
            let sep = c.str_req("separator")?;
            let target = field_opt(&doc, c, "target_field")?.unwrap_or_else(|| field.clone());
            let keep_trailing = c.bool_opt("preserve_trailing", false)?;
            let Some(text) = string_at(&doc, &field, ignore_missing)? else { return Ok(Some(doc)) };
            let re = regex::Regex::new(&sep).map_err(|e| IngestError::illegal(e.to_string()))?;
            let mut parts: Vec<Value> = re.split(&text).map(|s| json!(s)).collect();
            if !keep_trailing {
                while parts.last().and_then(|v| v.as_str()).map(|s| s.is_empty()).unwrap_or(false) {
                    parts.pop();
                }
            }
            doc.set(&target, Value::Array(parts)).map_err(IngestError::illegal)?;
        }
        "join" => {
            let field = field_of(&doc, c, "field")?;
            let sep = c.str_req("separator")?;
            let target = field_opt(&doc, c, "target_field")?.unwrap_or_else(|| field.clone());
            match doc.get(&field) {
                Some(Value::Array(a)) => {
                    let joined: Vec<String> = a.iter().map(super::hash::java_text).collect();
                    doc.set(&target, json!(joined.join(&sep))).map_err(IngestError::illegal)?;
                }
                Some(other) => {
                    return Err(IngestError::illegal(format!(
                        "field [{field}] of type [{}] cannot be cast to [java.util.List]",
                        type_name(&other)
                    )));
                }
                None => return Err(no_field(&field)),
            }
        }
        "sort" => {
            let field = field_of(&doc, c, "field")?;
            let target = field_opt(&doc, c, "target_field")?.unwrap_or_else(|| field.clone());
            let desc = c.str_opt("order")?.as_deref() == Some("desc");
            match doc.get(&field) {
                Some(Value::Array(mut a)) => {
                    a.sort_by(|x, y| compare_json(x, y));
                    if desc {
                        a.reverse();
                    }
                    doc.set(&target, Value::Array(a)).map_err(IngestError::illegal)?;
                }
                Some(other) => {
                    return Err(IngestError::illegal(format!(
                        "field [{field}] of type [{}] cannot be cast to [java.util.List]",
                        type_name(&other)
                    )));
                }
                None => {
                    if ignore_missing {
                        return Ok(Some(doc));
                    }
                    return Err(no_field(&field));
                }
            }
        }
        "convert" => {
            let field = field_of(&doc, c, "field")?;
            let target = field_opt(&doc, c, "target_field")?.unwrap_or_else(|| field.clone());
            let kind = c.str_req("type")?.to_lowercase();
            let Some(value) = doc.get(&field) else {
                if ignore_missing {
                    return Ok(Some(doc));
                }
                return Err(no_field(&field));
            };
            if value.is_null() {
                if ignore_missing {
                    return Ok(Some(doc));
                }
                return Err(field_null(&field));
            }
            let converted = match value {
                Value::Array(a) => {
                    let mut out = Vec::new();
                    for v in a {
                        out.push(convert(&v, &kind)?);
                    }
                    Value::Array(out)
                }
                other => convert(&other, &kind)?,
            };
            doc.set(&target, converted).map_err(IngestError::illegal)?;
        }
        "gsub" => {
            let field = field_of(&doc, c, "field")?;
            let target = field_opt(&doc, c, "target_field")?.unwrap_or_else(|| field.clone());
            let pattern = c.str_req("pattern")?;
            let replacement = c.str_req("replacement")?;
            let Some(text) = string_at(&doc, &field, ignore_missing)? else { return Ok(Some(doc)) };
            let re =
                regex::Regex::new(&pattern).map_err(|e| IngestError::illegal(e.to_string()))?;
            // Java writes a group as `$1`, which is what the regex crate reads
            let out = re.replace_all(&text, replacement.as_str()).to_string();
            doc.set(&target, json!(out)).map_err(IngestError::illegal)?;
        }
        "json" => {
            let field = field_of(&doc, c, "field")?;
            let target = field_opt(&doc, c, "target_field")?;
            let add_to_root = c.bool_opt("add_to_root", false)?;
            let strategy =
                c.str_opt("add_to_root_conflict_strategy")?.unwrap_or_else(|| "replace".into());
            let Some(value) = doc.get(&field) else { return Err(no_field(&field)) };
            let parsed: Value = match &value {
                Value::String(_) | Value::Number(_) | Value::Bool(_) => {
                    let text = super::hash::java_text(&value);
                    let mut de = serde_json::Deserializer::from_str(&text);
                    match serde::Deserialize::deserialize(&mut de) {
                        Ok(v) => v,
                        Err(e) => return Err(IngestError::illegal(format!("{e}"))),
                    }
                }
                Value::Null => Value::Null,
                other => {
                    return Err(IngestError::illegal(format!(
                        "field [{field}] of type [{}] cannot be cast to [java.lang.String]",
                        type_name(other)
                    )));
                }
            };
            if parsed.is_null() {
                doc.set(&target.unwrap_or(field), Value::Null).map_err(IngestError::illegal)?;
                return Ok(Some(doc));
            }
            if add_to_root {
                let Value::Object(o) = parsed else {
                    return Err(IngestError::illegal(
                        "cannot add non-map fields to root of document",
                    ));
                };
                for (k, v) in o {
                    if strategy == "merge"
                        && let (Some(Value::Object(mut existing)), Value::Object(incoming)) =
                            (doc.get(&k), v.clone())
                    {
                        for (ik, iv) in incoming {
                            existing.insert(ik, iv);
                        }
                        doc.set(&k, Value::Object(existing)).map_err(IngestError::illegal)?;
                        continue;
                    }
                    doc.set(&k, v).map_err(IngestError::illegal)?;
                }
            } else {
                doc.set(&target.unwrap_or(field), parsed).map_err(IngestError::illegal)?;
            }
        }
        "kv" => {
            let field = field_of(&doc, c, "field")?;
            let field_split = c.str_req("field_split")?;
            let value_split = c.str_req("value_split")?;
            let target = field_opt(&doc, c, "target_field")?;
            let include = c.strings_opt("include_keys")?;
            let exclude = c.strings_opt("exclude_keys")?.unwrap_or_default();
            let trim_key = c.str_opt("trim_key")?.unwrap_or_default();
            let trim_value = c.str_opt("trim_value")?.unwrap_or_default();
            let prefix = c.str_opt("prefix")?.unwrap_or_default();
            let strip = c.bool_opt("strip_brackets", false)?;
            let Some(text) = string_at(&doc, &field, ignore_missing)? else { return Ok(Some(doc)) };
            let fs =
                regex::Regex::new(&field_split).map_err(|e| IngestError::illegal(e.to_string()))?;
            let vs =
                regex::Regex::new(&value_split).map_err(|e| IngestError::illegal(e.to_string()))?;
            let trim_chars = |s: &str, set: &str| -> String {
                if set.is_empty() {
                    s.to_string()
                } else {
                    s.trim_matches(|ch| set.contains(ch)).to_string()
                }
            };
            let unbracket = |s: &str| -> String {
                if !strip {
                    return s.to_string();
                }
                let mut t = s.to_string();
                for (a, b) in [("(", ")"), ("[", "]"), ("<", ">"), ("\"", "\""), ("'", "'")] {
                    if t.starts_with(a) && t.ends_with(b) && t.len() >= 2 {
                        t = t[1..t.len() - 1].to_string();
                    }
                }
                t
            };
            for pair in fs.split(&text) {
                let mut kvs = vs.splitn(pair, 2);
                let (Some(k), Some(v)) = (kvs.next(), kvs.next()) else {
                    return Err(IngestError::illegal(format!(
                        "field [{field}] does not contain value_split [{value_split}]"
                    )));
                };
                let key = trim_chars(k, &trim_key);
                if let Some(inc) = &include
                    && !inc.contains(&key)
                {
                    continue;
                }
                if exclude.contains(&key) {
                    continue;
                }
                let value = unbracket(&trim_chars(v, &trim_value));
                let name = match &target {
                    Some(t) => format!("{t}.{prefix}{key}"),
                    None => format!("{prefix}{key}"),
                };
                match doc.get(&name) {
                    Some(Value::Array(mut a)) => {
                        a.push(json!(value));
                        doc.set(&name, Value::Array(a)).map_err(IngestError::illegal)?;
                    }
                    Some(existing) => {
                        doc.set(&name, json!([existing, value])).map_err(IngestError::illegal)?;
                    }
                    None => doc.set(&name, json!(value)).map_err(IngestError::illegal)?,
                }
            }
        }
        "csv" => {
            let field = field_of(&doc, c, "field")?;
            let targets = c.strings_req("target_fields")?;
            let sep = c.str_opt("separator")?.unwrap_or_else(|| ",".into());
            let quote = c.str_opt("quote")?.unwrap_or_else(|| "\"".into());
            let trim = c.bool_opt("trim", false)?;
            let empty = c.get("empty_value").cloned();
            let Some(text) = string_at(&doc, &field, ignore_missing)? else { return Ok(Some(doc)) };
            let sep_c = sep.chars().next().unwrap_or(',');
            let quote_c = quote.chars().next().unwrap_or('"');
            let mut values = parse_csv(&text, sep_c, quote_c, trim)?;
            if values.len() < targets.len()
                && let Some(e) = &empty
            {
                while values.len() < targets.len() {
                    values.push(super::hash::java_text(e));
                }
            }
            for (name, value) in targets.iter().zip(values) {
                doc.set(name, json!(value)).map_err(IngestError::illegal)?;
            }
        }
        "dot_expander" => {
            let field = c.str_req("field")?;
            let path = c.str_opt("path")?;
            let override_ = c.bool_opt("override", false)?;
            let holder_path = path.clone();
            let mut holder = match &holder_path {
                Some(p) => doc.get(p).unwrap_or(json!({})),
                None => doc.source.clone(),
            };
            if let Value::Object(o) = &mut holder {
                let expand_one = |o: &mut Map<String, Value>,
                                  name: &str|
                 -> Result<(), IngestError> {
                    let Some(value) = o.remove(name) else { return Ok(()) };
                    let mut target = Value::Object(std::mem::take(o));
                    // the value already at the path is merged or replaced
                    match (super::walk(&target, name).cloned(), value) {
                        (Some(Value::Object(mut existing)), Value::Object(incoming)) => {
                            for (k, v) in incoming {
                                if override_ || !existing.contains_key(&k) {
                                    existing.insert(k, v);
                                } else if let Some(Value::Array(mut a)) = existing.get(&k).cloned()
                                {
                                    a.push(v);
                                    existing.insert(k, Value::Array(a));
                                } else if let Some(prev) = existing.get(&k).cloned() {
                                    existing.insert(k, json!([prev, v]));
                                }
                            }
                            super::place(&mut target, name, Value::Object(existing))
                                .map_err(IngestError::illegal)?;
                        }
                        (Some(prev), v) if !override_ => {
                            let merged = match prev {
                                Value::Array(mut a) => {
                                    a.push(v);
                                    Value::Array(a)
                                }
                                other => json!([other, v]),
                            };
                            super::place(&mut target, name, merged)
                                .map_err(IngestError::illegal)?;
                        }
                        (_, v) => {
                            super::place(&mut target, name, v).map_err(IngestError::illegal)?
                        }
                    }
                    if let Value::Object(t) = target {
                        *o = t;
                    }
                    Ok(())
                };
                if field == "*" {
                    let names: Vec<String> =
                        o.keys().filter(|k| k.contains('.')).cloned().collect();
                    for n in names {
                        expand_one(o, &n)?;
                    }
                } else if o.contains_key(&field) {
                    expand_one(o, &field)?;
                }
            }
            match holder_path {
                Some(p) => doc.set(&p, holder).map_err(IngestError::illegal)?,
                None => doc.source = holder,
            }
        }
        "date" => {
            let field = field_of(&doc, c, "field")?;
            let target = field_opt(&doc, c, "target_field")?.unwrap_or_else(|| "@timestamp".into());
            let formats = c.strings_req("formats")?;
            let zone =
                c.str_opt("timezone")?.map(|z| doc.render(&z)).unwrap_or_else(|| "UTC".into());
            let output = c
                .str_opt("output_format")?
                .unwrap_or_else(|| "yyyy-MM-dd'T'HH:mm:ss.SSSXXX".into());
            let Some(value) = doc.get(&field) else { return Err(no_field(&field)) };
            let text = super::hash::java_text(&value);
            let mut parsed: Option<i64> = None;
            let mut last_err = String::new();
            for f in &formats {
                match parse_with_format(&text, f, &zone) {
                    Ok(ms) => {
                        parsed = Some(ms);
                        break;
                    }
                    Err(e) => last_err = e,
                }
            }
            let Some(ms) = parsed else {
                return Err(IngestError::illegal(format!(
                    "unable to parse date [{text}]{}",
                    if last_err.is_empty() { String::new() } else { format!(": {last_err}") }
                )));
            };
            let zone_ms = zone_offset_ms_at(&zone, ms);
            let rendered = format_in_zone(ms, &output, zone_ms).unwrap_or_else(|| text.clone());
            doc.set(&target, json!(rendered)).map_err(IngestError::illegal)?;
        }
        "date_index_name" => {
            let field = field_of(&doc, c, "field")?;
            let rounding = c.str_req("date_rounding")?;
            let prefix =
                c.str_opt("index_name_prefix")?.map(|p| doc.render(&p)).unwrap_or_default();
            let name_format = c
                .str_opt("index_name_format")?
                .map(|p| doc.render(&p))
                .unwrap_or_else(|| "yyyy-MM-dd".into());
            let zone = c.str_opt("timezone")?.unwrap_or_else(|| "UTC".into());
            let formats = c
                .strings_opt("date_formats")?
                .unwrap_or_else(|| vec!["yyyy-MM-dd'T'HH:mm:ss.SSSXX".into()]);
            let Some(value) = doc.get(&field) else { return Err(no_field(&field)) };
            let text = super::hash::java_text(&value);
            let mut parsed = None;
            for f in &formats {
                if let Ok(ms) = parse_with_format(&text, f, &zone) {
                    parsed = Some(ms);
                    break;
                }
            }
            let Some(ms) = parsed else {
                return Err(IngestError::illegal(format!("unable to parse date [{text}]")));
            };
            let zone_ms = zone_offset_ms(&zone);
            let rounded = round_down(ms + zone_ms, rounding_letter(&rounding)) - zone_ms;
            let stamp =
                crate::store::format_millis_at(rounded, &name_format, zone_ms).unwrap_or_default();
            doc.index = format!("{prefix}{stamp}");
        }
        "grok" => {
            let field = field_of(&doc, c, "field")?;
            let patterns = c.strings_req("patterns")?;
            let defs: HashMap<String, String> = c
                .map_opt("pattern_definitions")?
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            let trace = c.bool_opt("trace_match", false)?;
            let capture_all = c.bool_opt("capture_all_matches", false)?;
            let Some(text) = string_at(&doc, &field, ignore_missing)? else { return Ok(Some(doc)) };
            let mut matched = false;
            for (i, p) in patterns.iter().enumerate() {
                let g = super::grok::Grok::compile(p, &defs)?;
                if let Some(found) = g.captures(&text)? {
                    let mut written: Vec<String> = Vec::new();
                    for (name, value) in found {
                        if value.is_null() {
                            continue;
                        }
                        if written.contains(&name) {
                            // the same field captured again: kept as a list
                            // where every match is asked for, else the first
                            if capture_all {
                                let mut list = match doc.get(&name) {
                                    Some(Value::Array(a)) => a,
                                    Some(v) => vec![v],
                                    None => Vec::new(),
                                };
                                list.push(value);
                                doc.set(&name, Value::Array(list)).map_err(IngestError::illegal)?;
                            }
                            continue;
                        }
                        written.push(name.clone());
                        doc.set(&name, value).map_err(IngestError::illegal)?;
                    }
                    if trace {
                        doc.ingest.insert("_grok_match_index".into(), json!(i.to_string()));
                    }
                    matched = true;
                    break;
                }
            }
            if !matched {
                return Err(IngestError::illegal(
                    "Provided Grok expressions do not match field value: [".to_string()
                        + &text
                        + "]",
                ));
            }
        }
        "dissect" => {
            let field = field_of(&doc, c, "field")?;
            let pattern = c.str_req("pattern")?;
            let sep = c.str_opt("append_separator")?.unwrap_or_default();
            let Some(text) = string_at(&doc, &field, ignore_missing)? else { return Ok(Some(doc)) };
            let d = super::dissect::Dissect::compile(&pattern, &sep)?;
            for (name, value) in d.parse(&text)? {
                doc.set(&name, value).map_err(IngestError::illegal)?;
            }
        }
        "script" => {
            let mut script = json!({});
            for key in ["source", "inline", "id", "lang", "params"] {
                if let Some(v) = c.get(key) {
                    script[if key == "inline" { "source" } else { key }] = v.clone();
                }
            }
            let compiled =
                crate::painless::contexts::Compiled::of(&script, &|id| store.stored_script(id))
                    .map_err(|e| IngestError::of("script_exception", e.kind))?;
            let ctx = crate::painless::Value::from_json(&doc.as_ctx());
            let mut runner =
                crate::painless::contexts::Runner::new(&compiled.params).with_ctx(ctx.clone());
            runner.run(&compiled.script).map_err(|e| {
                let mut err = IngestError::of("script_exception", e.kind.to_string());
                err.reason = e.message.clone();
                err
            })?;
            if has_foreign_value(&ctx) {
                return Err(IngestError::illegal("Invalid data type for a document field"));
            }
            let back = ctx.try_json().map_err(|_| {
                IngestError::illegal("Iterable object is self-referencing itself (ingest script)")
            })?;
            doc.from_ctx(back);
        }
        "pipeline" => {
            let name = doc.render(&c.str_req("name")?);
            let ignore_missing_pipeline = c.bool_opt("ignore_missing_pipeline", false)?;
            match super::stored_pipeline(store, &name) {
                Some(Ok(p)) => {
                    // a pipeline that would run itself again is refused here,
                    // as this processor's own failure
                    if depth.contains(&name) {
                        return Err(IngestError::of(
                            "illegal_state_exception",
                            format!("Cycle detected for pipeline: {name}"),
                        ));
                    }
                    // the step for this processor comes first, then what the
                    // pipeline it named did; while that runs, `_ingest.pipeline`
                    // names it
                    steps.push(StepResult {
                        processor_type: "pipeline".into(),
                        tag: spec.tag.clone(),
                        description: spec.description.clone(),
                        status: "success",
                        // the step names the pipeline it ran; what that did
                        // to the document is in the steps that follow
                        doc: None,
                        error: None,
                        condition_met: None,
                        condition_text: None,
                    });
                    let outer = doc.ingest.get("pipeline").cloned();
                    let mut doc = doc;
                    doc.ingest.insert("pipeline".into(), json!(name));
                    let before = steps.len() - 1;
                    let mut inner_steps = Vec::new();
                    let out = super::run_pipeline(store, &p, doc, &mut inner_steps, depth);
                    steps.extend(inner_steps);
                    let out = out.map_err(|mut e| {
                        if e.kind == "illegal_state_exception" {
                            // a cycle is one failure, this processor's own
                            steps.truncate(before);
                            e.nested = false;
                        } else {
                            e.nested = true;
                        }
                        e
                    })?;
                    return Ok(out.map(|mut d| {
                        match outer {
                            Some(o) => {
                                d.ingest.insert("pipeline".into(), o);
                            }
                            None => {
                                d.ingest.remove("pipeline");
                            }
                        }
                        d
                    }));
                }
                Some(Err(e)) => return Err(e),
                None => {
                    if ignore_missing_pipeline {
                        return Ok(Some(doc));
                    }
                    return Err(IngestError::of(
                        "illegal_state_exception",
                        format!("Pipeline processor configured for non-existent pipeline [{name}]"),
                    ));
                }
            }
        }
        "drop" => return Ok(None),
        "fail" => {
            let message = doc.render(&c.str_req("message")?);
            return Err(IngestError::of("fail_processor_exception", message));
        }
        "foreach" => {
            let field = field_of(&doc, c, "field")?;
            let Some(Value::Object(p)) = c.get("processor") else {
                return Err(c.missing("processor"));
            };
            let inner = super::parse_processors(&[Value::Object(p.clone())])?;
            let Some(value) = doc.get(&field) else {
                if ignore_missing {
                    return Ok(Some(doc));
                }
                return Err(no_field(&field));
            };
            let mut out_items: Vec<Value> = Vec::new();
            let items: Vec<(Option<String>, Value)> = match value {
                Value::Array(a) => a.into_iter().map(|v| (None, v)).collect(),
                Value::Object(o) => o.into_iter().map(|(k, v)| (Some(k), v)).collect(),
                Value::Null => {
                    if ignore_missing {
                        return Ok(Some(doc));
                    }
                    return Err(field_null(&field));
                }
                other => {
                    return Err(IngestError::illegal(format!(
                        "field [{field}] of type [{}] cannot be cast to [java.util.List]",
                        type_name(&other)
                    )));
                }
            };
            let mut out_map: Map<String, Value> = Map::new();
            for (key, item) in items {
                doc.ingest.insert("_value".into(), item);
                if let Some(k) = &key {
                    doc.ingest.insert("_key".into(), json!(k));
                }
                let mut inner_steps = Vec::new();
                match super::run_processors(store, "_foreach", &inner, doc, &mut inner_steps, depth)
                {
                    Ok(Some(d)) => doc = d,
                    Ok(None) => return Ok(None),
                    Err((e, _)) => return Err(e),
                }
                let made = doc.ingest.remove("_value").unwrap_or(Value::Null);
                match key {
                    Some(k) => {
                        let k = doc
                            .ingest
                            .remove("_key")
                            .and_then(|v| v.as_str().map(|s| s.to_string()))
                            .unwrap_or(k);
                        out_map.insert(k, made);
                    }
                    None => out_items.push(made),
                }
            }
            doc.ingest.remove("_key");
            let rebuilt = if out_map.is_empty() && out_items.is_empty() {
                match doc.get(&field) {
                    Some(Value::Object(_)) => Value::Object(out_map),
                    _ => Value::Array(out_items),
                }
            } else if !out_map.is_empty() {
                Value::Object(out_map)
            } else {
                Value::Array(out_items)
            };
            doc.set(&field, rebuilt).map_err(IngestError::illegal)?;
        }
        "fingerprint" => {
            let fields = c.strings_opt("fields")?.unwrap_or_default();
            let exclude = c.strings_opt("exclude_fields")?.unwrap_or_default();
            let target =
                field_opt(&doc, c, "target_field")?.unwrap_or_else(|| "fingerprint".into());
            let method = c.str_opt("hash_method")?.unwrap_or_else(|| "SHA-1@2.16.0".into());
            if let Some(made) =
                super::hash::fingerprint(&doc, &fields, &exclude, &method, ignore_missing)?
            {
                doc.set(&target, json!(made)).map_err(IngestError::illegal)?;
            }
        }
        "community_id" => {
            run_community_id(c, &mut doc, ignore_missing)?;
        }
        "user_agent" => {
            let field = field_of(&doc, c, "field")?;
            let target = field_opt(&doc, c, "target_field")?.unwrap_or_else(|| "user_agent".into());
            let regex_file = c.str_opt("regex_file")?;
            let properties = c.strings_opt("properties")?;
            let Some(text) = string_at(&doc, &field, ignore_missing)? else { return Ok(Some(doc)) };
            let parser = super::user_agent::parser(regex_file.as_deref())?;
            let parsed = parser.parse(&text);
            doc.set(&target, parsed.to_json(&text, properties.as_deref()))
                .map_err(IngestError::illegal)?;
        }
        "attachment" => {
            let field = field_of(&doc, c, "field")?;
            let target = field_opt(&doc, c, "target_field")?.unwrap_or_else(|| "attachment".into());
            let properties = c.strings_opt("properties")?;
            // how much of a file is read. A field may carry the number for
            // one document, which is how a large file is read further than
            // the pipeline's own ceiling
            let mut limit = c
                .get("indexed_chars")
                .and_then(|v| v.as_i64())
                .unwrap_or(100_000);
            if let Some(from) = c.str_opt("indexed_chars_field")?
                && let Some(n) = doc.get(&from).and_then(|v| v.as_i64())
            {
                limit = n;
            }
            let limit = if limit < 0 { usize::MAX } else { limit as usize };
            let Some(encoded) = string_at(&doc, &field, ignore_missing)? else {
                return Ok(Some(doc));
            };
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .map_err(|_| {
                    IngestError::illegal(format!(
                        "field [{field}] is not a valid base64 value"
                    ))
                })?;
            let found = super::attachment::extract(&bytes, limit);
            let written = super::attachment::fields(&found, &found.content, properties.as_deref());
            doc.set(&target, written).map_err(IngestError::illegal)?;
        }
        "geoip" => {
            let field = field_of(&doc, c, "field")?;
            let target = field_opt(&doc, c, "target_field")?.unwrap_or_else(|| "geoip".into());
            let database =
                c.str_opt("database_file")?.unwrap_or_else(|| "GeoLite2-City.mmdb".into());
            let properties = c.strings_opt("properties")?;
            // the first address of a list is the one that stands for the
            // document, unless the processor was told to keep them all
            let first_only = c.bool_opt("first_only", true)?;
            if !doc.has(&field) {
                if ignore_missing {
                    return Ok(Some(doc));
                }
                return Err(no_field(&field));
            }
            let db = super::geoip::database(&database)?;
            if let Some(p) = properties.as_deref() {
                db.check(p)?;
            }
            let addresses: Vec<String> = match doc.get(&field) {
                Some(Value::Array(items)) => {
                    items.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()
                }
                Some(Value::String(one)) => vec![one.clone()],
                Some(_) => {
                    return Err(IngestError::illegal(format!(
                        "field [{field}] of type [java.lang.Object] cannot be cast to \
                         [java.lang.String]"
                    )));
                }
                None => Vec::new(),
            };
            let found: Vec<Option<Value>> =
                addresses.iter().map(|a| db.lookup(a, properties.as_deref())).collect();
            if first_only {
                // the first address anything is known about is the one that
                // stands for the document; the ones before it are passed over
                if let Some(one) = found.into_iter().flatten().next() {
                    doc.set(&target, one).map_err(IngestError::illegal)?;
                }
            } else if found.iter().any(|f| f.is_some()) {
                // every address keeps its place, so that the answers line up
                // with the addresses they came from; one nothing is known
                // about is a null in that place
                let all: Vec<Value> =
                    found.into_iter().map(|f| f.unwrap_or(Value::Null)).collect();
                doc.set(&target, Value::Array(all)).map_err(IngestError::illegal)?;
            }
        }
        other => {
            return Err(IngestError::illegal(format!(
                "No processor type exists with name [{other}]"
            )));
        }
    }
    Ok(Some(doc))
}

// ---- helpers ----------------------------------------------------------------

fn compare_json(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            x.as_f64().partial_cmp(&y.as_f64()).unwrap_or(std::cmp::Ordering::Equal)
        }
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => super::hash::java_text(a).cmp(&super::hash::java_text(b)),
    }
}

fn convert(v: &Value, kind: &str) -> Result<Value, IngestError> {
    let text = super::hash::java_text(v);
    Ok(match kind {
        "integer" => match v {
            Value::Number(n) if n.is_i64() => json!(n.as_i64().unwrap() as i32),
            _ => {
                let parsed = if let Some(h) = text.strip_prefix("0x") {
                    i64::from_str_radix(h, 16).ok()
                } else {
                    text.trim().parse::<i64>().ok()
                };
                match parsed {
                    Some(n) if n >= i32::MIN as i64 && n <= i32::MAX as i64 => json!(n),
                    _ => {
                        return Err(IngestError::illegal(format!(
                            "unable to convert [{text}] to integer"
                        )));
                    }
                }
            }
        },
        "long" => match v {
            Value::Number(n) if n.is_i64() => v.clone(),
            _ => match text.trim().parse::<i64>() {
                Ok(n) => json!(n),
                Err(_) => {
                    return Err(IngestError::illegal(format!(
                        "unable to convert [{text}] to long"
                    )));
                }
            },
        },
        "float" | "double" => match text.trim().parse::<f64>() {
            Ok(f) => {
                let f = if kind == "float" { f as f32 as f64 } else { f };
                json!(f)
            }
            Err(_) => {
                return Err(IngestError::illegal(format!("unable to convert [{text}] to {kind}")));
            }
        },
        "string" => json!(text),
        "boolean" => match text.trim().to_lowercase().as_str() {
            "true" => json!(true),
            "false" => json!(false),
            _ => {
                return Err(IngestError::illegal(format!(
                    "[{text}] is not a boolean value, cannot convert to boolean"
                )));
            }
        },
        "ip" => {
            if text.parse::<std::net::IpAddr>().is_ok() {
                json!(text)
            } else {
                return Err(IngestError::illegal(format!(
                    "[{text}] is not a valid ipv4/ipv6 address"
                )));
            }
        }
        _ => {
            // auto: a boolean, an integer, a long, a double, else the text
            match text.trim().to_lowercase().as_str() {
                "true" => json!(true),
                "false" => json!(false),
                t => {
                    if let Ok(n) = t.parse::<i64>() {
                        if n >= i32::MIN as i64 && n <= i32::MAX as i64 {
                            json!(n)
                        } else {
                            json!(n)
                        }
                    } else if let Ok(f) = t.parse::<f64>() {
                        json!(f)
                    } else {
                        json!(text)
                    }
                }
            }
        }
    })
}

pub(crate) fn bytes_of(s: &str) -> Result<i64, IngestError> {
    let t = s.trim().to_lowercase();
    let (num, unit) = match t.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => (t[..i].trim(), t[i..].trim()),
        None => (t.as_str(), ""),
    };
    let n: f64 = num
        .parse()
        .map_err(|_| IngestError::illegal(format!("failed to parse setting [Ingest Field] with value [{s}] as a size in bytes: unit is missing or unrecognized")))?;
    let mult: f64 = match unit {
        "" | "b" => 1.0,
        "kb" | "k" => 1024.0,
        "mb" | "m" => 1024.0 * 1024.0,
        "gb" | "g" => 1024.0 * 1024.0 * 1024.0,
        "tb" | "t" => 1024f64.powi(4),
        "pb" | "p" => 1024f64.powi(5),
        _ => {
            return Err(IngestError::illegal(format!(
                "failed to parse setting [Ingest Field] with value [{s}] as a size in bytes: unit is missing or unrecognized"
            )));
        }
    };
    if unit.is_empty() && n.fract() != 0.0 {
        return Err(IngestError::illegal(format!(
            "failed to parse setting [Ingest Field] with value [{s}] as a size in bytes"
        )));
    }
    Ok((n * mult) as i64)
}

fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut tag = String::new();
    for c in s.chars() {
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                let name: String = tag
                    .trim_start_matches('/')
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect::<String>()
                    .to_lowercase();
                if !matches!(
                    name.as_str(),
                    "b" | "i"
                        | "u"
                        | "a"
                        | "em"
                        | "strong"
                        | "span"
                        | "font"
                        | "code"
                        | "small"
                        | "big"
                        | "sub"
                        | "sup"
                        | "s"
                        | "strike"
                        | "tt"
                ) {
                    out.push('\n');
                }
            }
            _ if !in_tag => out.push(c),
            _ => tag.push(c),
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&nbsp;", " ")
}

fn parse_csv(text: &str, sep: char, quote: char, trim: bool) -> Result<Vec<String>, IngestError> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    let mut was_quoted = false;
    while let Some(c) = chars.next() {
        if quoted {
            if c == quote {
                if chars.peek() == Some(&quote) {
                    cur.push(quote);
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                cur.push(c);
            }
        } else if c == quote && cur.trim().is_empty() {
            quoted = true;
            was_quoted = true;
            cur.clear();
        } else if c == sep {
            out.push(if trim && !was_quoted { cur.trim().to_string() } else { cur.clone() });
            cur.clear();
            was_quoted = false;
        } else {
            cur.push(c);
        }
    }
    if quoted {
        return Err(IngestError::illegal("Unclosed quote"));
    }
    out.push(if trim && !was_quoted { cur.trim().to_string() } else { cur });
    Ok(out)
}

fn zone_offset_ms(zone: &str) -> i64 {
    zone_offset_ms_at(zone, 0)
}

fn zone_offset_ms_at(zone: &str, at_ms: i64) -> i64 {
    let z = zone.trim();
    if z.is_empty() || z == "UTC" || z == "Z" || z == "GMT" {
        return 0;
    }
    if let Some(rest) = z.strip_prefix('+').or_else(|| z.strip_prefix('-')) {
        let sign = if z.starts_with('-') { -1 } else { 1 };
        let (h, m) = match rest.split_once(':') {
            Some((h, m)) => (h.parse::<i64>().unwrap_or(0), m.parse::<i64>().unwrap_or(0)),
            None if rest.len() == 4 => {
                (rest[..2].parse().unwrap_or(0), rest[2..].parse().unwrap_or(0))
            }
            None => (rest.parse::<i64>().unwrap_or(0), 0),
        };
        return sign * (h * 3_600_000 + m * 60_000);
    }
    crate::tz::offset_at(z, at_ms.div_euclid(1000)).map(|secs| secs as i64 * 1000).unwrap_or(0)
}

/// Read a date the way a named or a Java pattern format reads it, into
/// milliseconds since the epoch; a zone applies where the text names none.
fn parse_with_format(text: &str, format: &str, zone: &str) -> Result<i64, String> {
    let t = text.trim();
    let ms = match format {
        "UNIX" => t
            .parse::<f64>()
            .map(|s| (s * 1000.0) as i64)
            .map_err(|_| "not a unix time".to_string())?,
        "UNIX_MS" => t.parse::<i64>().map_err(|_| "not a unix time in millis".to_string())?,
        "TAI64N" => {
            let hex = t.trim_start_matches('@');
            let secs = i64::from_str_radix(&hex[..16.min(hex.len())], 16)
                .map_err(|_| "not a TAI64N".to_string())?;
            let nanos =
                if hex.len() > 16 { i64::from_str_radix(&hex[16..], 16).unwrap_or(0) } else { 0 };
            (secs - (1i64 << 62) - 10) * 1000 + nanos / 1_000_000
        }
        "ISO8601"
        | "strict_date_optional_time"
        | "date_optional_time"
        | "strict_date_time"
        | "date_time"
        | "strict_date_time_no_millis"
        | "date_time_no_millis"
        | "strict_date"
        | "date"
        | "basic_date"
        | "basic_date_time"
        | "strict_date_hour_minute_second" => {
            // a zone written without its colon, or a space for the T
            let mut spelled = t.replace(' ', "T");
            if spelled.len() > 5 {
                let tail = &spelled[spelled.len() - 5..];
                if (tail.starts_with('+') || tail.starts_with('-'))
                    && tail[1..].chars().all(|c| c.is_ascii_digit())
                {
                    spelled =
                        format!("{}{}:{}", &spelled[..spelled.len() - 5], &tail[..3], &tail[3..]);
                }
            }
            let dt = crate::store::parse_date_lenient(&spelled)
                .ok_or_else(|| format!("Text '{t}' could not be parsed"))?;
            let ms = (dt.unix_timestamp_nanos() / 1_000_000) as i64;
            // a text with no zone of its own is read in the zone given
            if !has_zone(t) { ms - zone_offset_ms_at(zone, ms) } else { ms }
        }
        pattern => {
            let dt = crate::store::parse_with_pattern(t, pattern)
                .ok_or_else(|| format!("Text '{t}' could not be parsed at index 0"))?;
            let ms = (dt.unix_timestamp_nanos() / 1_000_000) as i64;
            let zoned = pattern.contains('X')
                || pattern.contains('Z')
                || pattern.contains('z')
                || pattern.contains("VV");
            if !zoned { ms - zone_offset_ms_at(zone, ms) } else { ms }
        }
    };
    Ok(ms)
}

/// An instant written with a pattern, in a zone: the pattern's zone letters
/// write the zone's offset.
fn format_in_zone(ms: i64, pattern: &str, zone_ms: i64) -> Option<String> {
    let named = matches!(
        pattern,
        "iso8601"
            | "ISO8601"
            | "strict_date_optional_time"
            | "date_optional_time"
            | "date_time"
            | "strict_date_time"
            | "strict_date"
            | "date"
            | "basic_date"
            | "epoch_millis"
            | "epoch_second"
    );
    if named {
        return crate::store::format_millis_at(ms, pattern, zone_ms);
    }
    let offset = boostcore::time::UtcOffset::from_whole_seconds((zone_ms / 1000) as i32).ok()?;
    let local = boostcore::time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .ok()?
        .to_offset(offset);
    Some(crate::store::format_with_pattern(local, pattern))
}

fn has_zone(t: &str) -> bool {
    t.ends_with('Z') || {
        let tail = &t[t.len().saturating_sub(6)..];
        (tail.starts_with('+') || tail.starts_with('-')) && tail.contains(':')
    }
}

/// The start of the unit an instant falls in.
fn round_down(ms: i64, unit: &str) -> i64 {
    let day = 86_400_000i64;
    match unit {
        "s" => ms.div_euclid(1000) * 1000,
        "m" => ms.div_euclid(60_000) * 60_000,
        "h" => ms.div_euclid(3_600_000) * 3_600_000,
        "d" => ms.div_euclid(day) * day,
        "w" => {
            // weeks start on Monday; 1970-01-01 was a Thursday
            let days = ms.div_euclid(day);
            let since_monday = (days + 3).rem_euclid(7);
            (days - since_monday) * day
        }
        "M" | "y" => {
            let days = ms.div_euclid(day);
            let (y, m, _) = crate::painless::value::civil_from_days(days);
            let first = if unit == "y" { days_of(y, 1, 1) } else { days_of(y, m, 1) };
            first * day
        }
        _ => ms,
    }
}

fn days_of(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn rounding_letter(r: &str) -> &str {
    match r {
        "y" | "year" => "y",
        "M" | "month" => "M",
        "w" | "week" => "w",
        "d" | "day" => "d",
        "h" | "hour" => "h",
        "m" | "minute" => "m",
        "s" | "second" => "s",
        other => other,
    }
}

fn run_community_id(c: &Cfg, doc: &mut IngestDoc, ignore_missing: bool) -> Result<(), IngestError> {
    let src_ip_f = c.str_req("source_ip_field")?;
    let dst_ip_f = c.str_req("destination_ip_field")?;
    let src_port_f = c.str_opt("source_port_field")?;
    let dst_port_f = c.str_opt("destination_port_field")?;
    let iana_f = c.str_opt("iana_protocol_number_field")?;
    let proto_f = c.str_opt("protocol_field")?;
    let icmp_type_f = c.str_opt("icmp_type_field")?;
    let icmp_code_f = c.str_opt("icmp_code_field")?;
    let seed = c.int_opt("seed")?.unwrap_or(0) as u16;
    let target = c.str_opt("target_field")?.unwrap_or_else(|| "community_id".into());

    // the protocol, by number or by name
    let mut proto: Option<u8> = None;
    if let Some(f) = &iana_f {
        match doc.get(f) {
            Some(Value::Number(n)) => proto = n.as_i64().map(|n| n as u8),
            Some(Value::String(s)) if !s.is_empty() => proto = s.parse::<u8>().ok(),
            _ => {}
        }
    }
    if proto.is_none()
        && let Some(f) = &proto_f
    {
        let name = match doc.get(f) {
            Some(Value::String(s)) => s.to_lowercase(),
            Some(Value::Null) | None => {
                if ignore_missing {
                    return Ok(());
                }
                return Err(IngestError::illegal(format!(
                    "network protocol in the field [{f}] is null or empty"
                )));
            }
            Some(other) => super::hash::java_text(&other).to_lowercase(),
        };
        proto = Some(match name.as_str() {
            "icmp" => 1,
            "tcp" => 6,
            "udp" => 17,
            "icmp-v6" | "icmpv6" | "ipv6-icmp" | "icmp_v6" => 58,
            "sctp" => 132,
            "" => {
                if ignore_missing {
                    return Ok(());
                }
                return Err(IngestError::illegal(format!(
                    "network protocol in the field [{f}] is null or empty"
                )));
            }
            other => return Err(IngestError::illegal(format!("unsupported protocol [{other}]"))),
        });
    }
    let Some(proto) = proto else {
        if ignore_missing {
            return Ok(());
        }
        return Err(IngestError::illegal("unsupported protocol"));
    };
    let ip_of = |f: &str| -> Result<Option<Vec<u8>>, IngestError> {
        match doc.get(f) {
            Some(Value::String(s)) if !s.is_empty() => match s.parse::<std::net::IpAddr>() {
                Ok(std::net::IpAddr::V4(v4)) => Ok(Some(v4.octets().to_vec())),
                Ok(std::net::IpAddr::V6(v6)) => Ok(Some(v6.octets().to_vec())),
                Err(_) => Err(IngestError::illegal(format!(
                    "ip address in the field [{f}] is not a valid ipv4/ipv6 address"
                ))),
            },
            _ => {
                if ignore_missing {
                    Ok(None)
                } else {
                    Err(IngestError::illegal(format!(
                        "ip address in the field [{f}] is null or empty"
                    )))
                }
            }
        }
    };
    let Some(sip) = ip_of(&src_ip_f)? else { return Ok(()) };
    let Some(dip) = ip_of(&dst_ip_f)? else { return Ok(()) };
    if sip.len() != dip.len() {
        return Err(IngestError::illegal("source ip and destination ip must have same format"));
    }
    let port_of = |f: &Option<String>| -> Result<Option<u16>, IngestError> {
        let Some(f) = f else {
            return if ignore_missing {
                Ok(None)
            } else {
                Err(IngestError::illegal("port field is missing"))
            };
        };
        match doc.get(f) {
            Some(Value::Number(n)) => match n.as_i64() {
                Some(p) if (0..=65535).contains(&p) => Ok(Some(p as u16)),
                _ => Err(IngestError::illegal(format!(
                    "both source port and destination port must be between 0 and 65535, but port in the field [{f}] is [{n}]"
                ))),
            },
            Some(Value::String(s)) if !s.is_empty() => s.parse::<u16>().map(Some).map_err(|_| {
                IngestError::illegal(format!("port in the field [{f}] is not a number"))
            }),
            _ => {
                if ignore_missing {
                    Ok(None)
                } else {
                    Err(IngestError::illegal(format!("port in the field [{f}] is null or empty")))
                }
            }
        }
    };
    let transport = matches!(proto, 6 | 17 | 132);
    let icmp = matches!(proto, 1 | 58);
    let (mut sport, mut dport) = (0u16, 0u16);
    if transport {
        let Some(s) = port_of(&src_port_f)? else { return Ok(()) };
        let Some(d) = port_of(&dst_port_f)? else { return Ok(()) };
        sport = s;
        dport = d;
    }
    let mut one_way = true;
    if icmp {
        let icmp_val = |f: &Option<String>, what: &str| -> Result<Option<u8>, IngestError> {
            let Some(f) = f else {
                return if ignore_missing {
                    Ok(None)
                } else {
                    Err(IngestError::illegal(format!("icmp message {what} field is missing")))
                };
            };
            match doc.get(f) {
                Some(Value::Number(n)) => Ok(n.as_i64().map(|v| v as u8)),
                Some(Value::String(s)) if !s.is_empty() => Ok(s.parse::<u8>().ok()),
                _ => {
                    if ignore_missing {
                        Ok(None)
                    } else {
                        Err(IngestError::illegal(format!(
                            "icmp message {what} in the field [{f}] is null or empty"
                        )))
                    }
                }
            }
        };
        let Some(ty) = icmp_val(&icmp_type_f, "type")? else { return Ok(()) };
        sport = ty as u16;
        match super::hash::icmp_equivalent(proto == 58, ty) {
            Some(code) => {
                one_way = false;
                dport = code as u16;
            }
            None => {
                let Some(code) = icmp_val(&icmp_code_f, "code")? else { return Ok(()) };
                dport = code as u16;
            }
        }
    }
    // the lesser side goes first, so both directions hash alike
    let is_less = match sip.cmp(&dip) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => sport < dport,
    };
    let swap = !is_less && (!icmp || !one_way);
    let id = super::hash::community_id(&sip, &dip, sport, dport, proto, seed, swap);
    doc.set(&target, json!(id)).map_err(IngestError::illegal)
}

/// Whether a value holds something no document can: a pattern, a lambda,
/// a native object.
fn has_foreign_value(v: &crate::painless::Value) -> bool {
    let mut seen: Vec<usize> = Vec::new();
    foreign_within(v, &mut seen)
}

fn foreign_within(v: &crate::painless::Value, seen: &mut Vec<usize>) -> bool {
    use crate::painless::Value as V;
    match v {
        // a char is a Painless type with no JSON of its own: writing one into
        // a document would have to guess whether it meant a string or a
        // number, so OpenSearch refuses it and so does this
        V::Regex(_) | V::Lambda(_) | V::Native(_) | V::Builder(_) | V::Error(_) | V::Char(_) => {
            true
        }
        V::List(l) => {
            let addr = std::rc::Rc::as_ptr(l) as *const () as usize;
            if seen.contains(&addr) {
                return false;
            }
            seen.push(addr);
            l.borrow().iter().any(|x| foreign_within(x, seen))
        }
        V::Map(m) => {
            let addr = std::rc::Rc::as_ptr(m) as *const () as usize;
            if seen.contains(&addr) {
                return false;
            }
            seen.push(addr);
            m.borrow().iter().any(|(_, x)| foreign_within(x, seen))
        }
        _ => false,
    }
}
