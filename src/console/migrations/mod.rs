//! Bringing an object written by an older console up to date.
//!
//! Every type keeps a chain of changes to its own shape, each named for the
//! version that made it: a dashboard from 6.x kept its filters in the search
//! source and its panel layout in `uiStateJSON`, and 7.0 moved the index
//! pattern it read into a reference, and 7.3 moved the filters into the query
//! and the layout into each panel. An object records which of those it has
//! been through, and bringing it up to date is running the ones it has not.
//!
//! These are ports. Each function here is the one in OpenSearch Dashboards
//! with the same name, in `src/plugins/*/server/saved_objects/`, and it does
//! what that one does including the parts that look accidental -- a migration
//! that changed behaviour would be a migration that made a different
//! dashboard out of the same file. Where the original swallows an error and
//! hands the document back untouched, so does this.
//!
//! What runs and in what order is the rule the core migrator uses: a document
//! with no record at all of what it has been through is assumed to have been
//! through nothing, and every change its type knows about is run in the order
//! of the versions that made them. One that records a version is run through
//! the changes after that version only.

pub mod config;
pub mod dashboard;
pub mod index_pattern;
pub mod search;
pub mod visualization;

use serde_json::{Map, Value, json};

/// One change to a type's shape, and the version that made it.
pub struct Step {
    pub version: &'static str,
    pub apply: fn(Value) -> Value,
}

/// The chain for a type, if it has one.
pub fn chain_for(kind: &str) -> Option<&'static [Step]> {
    Some(match kind {
        "dashboard" => dashboard::CHAIN,
        "visualization" => visualization::CHAIN,
        "index-pattern" => index_pattern::CHAIN,
        "search" => search::CHAIN,
        "config" => config::CHAIN,
        _ => return None,
    })
}

/// The latest version each type's chain reaches.
pub fn latest(kind: &str) -> Option<&'static str> {
    chain_for(kind).and_then(|chain| chain.last()).map(|step| step.version)
}

/// Bring a saved object up to date.
///
/// The object is in the form the API speaks -- `id`, `type`, `attributes`,
/// `references`, `migrationVersion` -- rather than as a raw document, because
/// that is the form every migration was written against.
pub fn migrate(mut doc: Value) -> Result<Value, String> {
    let kind = doc.get("type").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let Some(chain) = chain_for(&kind) else {
        return Ok(doc);
    };
    // what it has been through already, if it says
    let done = doc
        .pointer(&format!("/migrationVersion/{kind}"))
        .and_then(|v| v.as_str())
        .map(String::from);
    if let Some(done) = &done
        && let Some(last) = chain.last()
        && newer(done, last.version)
    {
        return Err(format!(
            "Document \"{}\" has property \"{kind}\" which belongs to a more recent version of \
             OpenSearch Dashboards [{done}]. The last known version is [{}]",
            doc.get("id").and_then(|v| v.as_str()).unwrap_or_default(),
            last.version
        ));
    }
    for step in chain {
        if done.as_deref().is_some_and(|d| !newer(step.version, d)) {
            continue;
        }
        doc = (step.apply)(doc);
        // the record moves forward as each change lands, so a change that
        // fails halfway leaves an honest record rather than none
        let versions = doc.get_mut("migrationVersion").and_then(|v| v.as_object_mut()).map(|m| {
            m.insert(kind.clone(), json!(step.version));
        });
        if versions.is_none() {
            doc["migrationVersion"] = json!({kind.clone(): step.version});
        }
    }
    Ok(doc)
}

/// Whether one version comes after another, the way semver orders them.
pub fn newer(a: &str, b: &str) -> bool {
    let parse =
        |v: &str| -> Vec<u64> { v.split('.').map(|p| p.trim().parse().unwrap_or(0)).collect() };
    parse(a) > parse(b)
}

// ---- the small things every migration does ----------------------------------

/// `get(doc, 'a.b.c')`: the value at a path, or nothing.
pub fn at<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(doc, |here, key| here.get(key))
}

/// The JSON a string field holds, parsed, or nothing where it is not a string
/// or does not parse -- which every migration treats as "leave it alone".
pub fn parsed(doc: &Value, path: &str) -> Option<Value> {
    at(doc, path).and_then(|v| v.as_str()).and_then(|s| serde_json::from_str(s).ok())
}

/// A value written back at a path, making the objects on the way.
pub fn set(doc: &mut Value, path: &str, value: Value) {
    let mut here = doc;
    let parts: Vec<&str> = path.split('.').collect();
    for key in &parts[..parts.len() - 1] {
        if !here.get(*key).is_some_and(|v| v.is_object()) {
            here[*key] = json!({});
        }
        here = &mut here[*key];
    }
    here[parts[parts.len() - 1]] = value;
}

/// A JSON value written back as the string a field holds.
pub fn set_json(doc: &mut Value, path: &str, value: &Value) {
    set(doc, path, json!(value.to_string()));
}

/// The references list, made if there is none.
pub fn references(doc: &mut Value) -> &mut Vec<Value> {
    if !doc.get("references").is_some_and(|v| v.is_array()) {
        doc["references"] = json!([]);
    }
    doc["references"].as_array_mut().expect("just made")
}

/// The attributes, as a map to change.
pub fn attributes(doc: &mut Value) -> &mut Map<String, Value> {
    if !doc.get("attributes").is_some_and(|v| v.is_object()) {
        doc["attributes"] = json!({});
    }
    doc["attributes"].as_object_mut().expect("just made")
}

/// The one move three types make in 7.0: the index pattern a search source
/// names becomes a reference, so an export can carry it and an import can
/// renumber it.
pub fn migrate_index_pattern(doc: &mut Value) {
    let Some(mut source) = parsed(doc, "attributes.kibanaSavedObjectMeta.searchSourceJSON") else {
        return;
    };
    if let Some(index) = source.get("index").cloned()
        && doc.get("references").is_some_and(|v| v.is_array())
    {
        source["indexRefName"] = json!("kibanaSavedObjectMeta.searchSourceJSON.index");
        references(doc).push(json!({
            "name": "kibanaSavedObjectMeta.searchSourceJSON.index",
            "type": "index-pattern",
            "id": index,
        }));
        source.as_object_mut().map(|o| o.remove("index"));
    }
    if let Some(filters) = source.get_mut("filter").and_then(|v| v.as_array_mut()) {
        let mut found = Vec::new();
        for (i, row) in filters.iter_mut().enumerate() {
            let Some(index) = row.pointer("/meta/index").cloned().filter(|v| !v.is_null()) else {
                continue;
            };
            let name = format!("kibanaSavedObjectMeta.searchSourceJSON.filter[{i}].meta.index");
            row["meta"]["indexRefName"] = json!(name);
            row["meta"].as_object_mut().map(|m| m.remove("index"));
            found.push(json!({"name": name, "type": "index-pattern", "id": index}));
        }
        if doc.get("references").is_some_and(|v| v.is_array()) {
            references(doc).extend(found);
        }
    }
    set_json(doc, "attributes.kibanaSavedObjectMeta.searchSourceJSON", &source);
}

/// A search source whose query is `match_all` is a search source with no
/// query, written the way the query bar writes an empty one.
pub fn migrate_match_all_query(mut doc: Value) -> Value {
    let Some(mut source) = parsed(&doc, "attributes.kibanaSavedObjectMeta.searchSourceJSON") else {
        return doc;
    };
    if source.pointer("/query/match_all").is_none() {
        return doc;
    }
    source["query"] = json!({"query": "", "language": "kuery"});
    // the original writes the meta afresh, with only the search source in it
    set(
        &mut doc,
        "attributes.kibanaSavedObjectMeta",
        json!({"searchSourceJSON": source.to_string()}),
    );
    doc
}

// ---- raw documents and saved objects ------------------------------------------

/// Whether a document in the index is a saved object at all: its id says
/// which type and namespace it is, and its source agrees.
pub fn is_saved_object(id: &str, source: &Value) -> bool {
    let Some(kind) = source.get("type").and_then(|v| v.as_str()) else { return false };
    let prefix = match source.get("namespace").and_then(|v| v.as_str()) {
        Some(ns) => format!("{ns}:{kind}:"),
        None => format!("{kind}:"),
    };
    id.starts_with(&prefix) && source.get(kind).is_some()
}

/// A raw document as the object it is.
pub fn from_raw(id: &str, source: &Value) -> Value {
    let kind = source.get("type").and_then(|v| v.as_str()).unwrap_or_default();
    let namespace = source.get("namespace").and_then(|v| v.as_str());
    let prefix = match namespace {
        Some(ns) => format!("{ns}:{kind}:"),
        None => format!("{kind}:"),
    };
    let mut out = Map::new();
    out.insert("type".into(), json!(kind));
    out.insert("id".into(), json!(id.strip_prefix(&prefix).unwrap_or(id)));
    if let Some(ns) = namespace {
        out.insert("namespace".into(), json!(ns));
    }
    out.insert("attributes".into(), source.get(kind).cloned().unwrap_or_else(|| json!({})));
    out.insert("references".into(), source.get("references").cloned().unwrap_or_else(|| json!([])));
    if let Some(v) = source.get("migrationVersion") {
        out.insert("migrationVersion".into(), v.clone());
    }
    if let Some(v) = source.get("updated_at") {
        out.insert("updated_at".into(), v.clone());
    }
    Value::Object(out)
}

/// An object as the raw document it is written as.
pub fn to_raw(doc: &Value) -> (String, Value) {
    let kind = doc.get("type").and_then(|v| v.as_str()).unwrap_or_default();
    let id = doc.get("id").and_then(|v| v.as_str()).unwrap_or_default();
    let namespace = doc.get("namespace").and_then(|v| v.as_str());
    let raw_id = match namespace {
        Some(ns) => format!("{ns}:{kind}:{id}"),
        None => format!("{kind}:{id}"),
    };
    let mut source = Map::new();
    source.insert(kind.to_string(), doc.get("attributes").cloned().unwrap_or_else(|| json!({})));
    source.insert("type".into(), json!(kind));
    source.insert("references".into(), doc.get("references").cloned().unwrap_or_else(|| json!([])));
    if let Some(ns) = namespace {
        source.insert("namespace".into(), json!(ns));
    }
    if let Some(v) =
        doc.get("migrationVersion").filter(|v| v.as_object().is_some_and(|o| !o.is_empty()))
    {
        source.insert("migrationVersion".into(), v.clone());
    }
    if let Some(v) = doc.get("updated_at") {
        source.insert("updated_at".into(), v.clone());
    }
    (raw_id, Value::Object(source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_as_numbers_not_as_text() {
        assert!(newer("7.10.0", "7.9.3"), "ten is after nine");
        assert!(!newer("7.9.3", "7.10.0"));
        assert!(!newer("7.0.0", "7.0.0"));
    }

    #[test]
    fn a_document_with_no_record_goes_through_everything() {
        let doc = json!({"id": "d1", "type": "dashboard", "attributes": {"panelsJSON": "[]"},
                         "references": [], "migrationVersion": {}});
        let out = migrate(doc).expect("migrates");
        assert_eq!(out["migrationVersion"]["dashboard"], "7.9.3");
    }

    #[test]
    fn a_document_from_the_future_is_refused() {
        let doc = json!({"id": "d1", "type": "dashboard", "attributes": {},
                         "references": [], "migrationVersion": {"dashboard": "99.0.0"}});
        assert!(migrate(doc).is_err());
    }

    #[test]
    fn a_document_partly_through_runs_only_what_is_left() {
        // 7.0.0 would turn a panel's type and id into a reference; a document
        // that says it has been through 7.3.0 keeps them
        let doc = json!({"id": "d1", "type": "dashboard",
                         "attributes": {"panelsJSON": "[{\"type\":\"visualization\",\"id\":\"v\"}]"},
                         "references": [], "migrationVersion": {"dashboard": "7.3.0"}});
        let out = migrate(doc).expect("migrates");
        assert!(out["attributes"]["panelsJSON"].as_str().unwrap().contains("\"id\":\"v\""));
        assert_eq!(out["migrationVersion"]["dashboard"], "7.9.3");
    }

    #[test]
    fn a_raw_document_names_its_namespace_in_its_id() {
        let source = json!({"type": "config", "namespace": "foo-ns", "config": {"buildNum": 1}});
        assert!(is_saved_object("foo-ns:config:7.0.0", &source));
        assert!(!is_saved_object("config:7.0.0", &source), "the namespace is missing from the id");
        let doc = from_raw("foo-ns:config:7.0.0", &source);
        assert_eq!(doc["id"], "7.0.0");
        assert_eq!(doc["namespace"], "foo-ns");
        let (id, back) = to_raw(&doc);
        assert_eq!(id, "foo-ns:config:7.0.0");
        assert_eq!(back["config"]["buildNum"], 1);
    }

    #[test]
    fn a_match_all_query_becomes_an_empty_one() {
        let doc = json!({"attributes": {"kibanaSavedObjectMeta": {
            "searchSourceJSON": "{\"query\":{\"match_all\":{}},\"filter\":[]}"}}});
        let out = migrate_match_all_query(doc);
        let source: Value = serde_json::from_str(
            out["attributes"]["kibanaSavedObjectMeta"]["searchSourceJSON"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(source["query"], json!({"query": "", "language": "kuery"}));
        assert_eq!(source["filter"], json!([]), "the rest is kept");
    }
}
