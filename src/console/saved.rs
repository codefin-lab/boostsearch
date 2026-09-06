//! Saved objects: the things somebody made in the console and expects to
//! still be there tomorrow.
//!
//! An index pattern, a visualization, a dashboard, a search — each is a
//! document in the console's own index with an id of `{type}:{id}`, its
//! attributes under a property named after its type, and a list of the other
//! objects it points at. That last part is what makes a dashboard portable:
//! it names the visualizations it shows by reference rather than by id, so
//! exporting one and importing it somewhere else can renumber everything and
//! still have it draw.

use base64::Engine as _;
use serde_json::{Map, Value, json};

use super::engine::{Engine, Failed, INDEX};

/// The store, and what it knows about the types in it.
pub struct Saved<'a> {
    pub engine: &'a Engine,
    /// what version of itself each type's attributes are written at
    pub migration_versions: &'a std::collections::BTreeMap<String, Value>,
    /// the shape the index should have, for putting it back when something
    /// has taken it away
    pub mapping: &'a Value,
}

/// What a caller asked to write.
pub struct Writing {
    pub kind: String,
    pub id: Option<String>,
    pub attributes: Value,
    pub references: Value,
    pub migration_version: Option<Value>,
    pub overwrite: bool,
}

impl<'a> Saved<'a> {
    pub fn new(
        engine: &'a Engine,
        migration_versions: &'a std::collections::BTreeMap<String, Value>,
        mapping: &'a Value,
    ) -> Saved<'a> {
        Saved { engine, migration_versions, mapping }
    }

    /// Put the index back if something has taken it away.
    ///
    /// A write to an alias that is not there would have the engine make a
    /// plain index under the alias's name, which is the one arrangement a
    /// console cannot work in -- so writes say the target must be an alias,
    /// and this is what happens when one is refused for that reason.
    fn make_it_right(&self) -> Result<(), Failed> {
        super::migrate::ensure(self.engine, self.mapping).map(|_| ())
    }

    /// One object, as the API hands it back.
    pub fn get(&self, kind: &str, id: &str) -> Result<Value, Failed> {
        let found =
            self.engine.call("GET", &format!("/{INDEX}/_doc/{}", document_id(kind, id)), None)?;
        match found.get("found").and_then(|v| v.as_bool()) {
            Some(true) => Ok(shape(&found, kind, id)),
            _ => Err(missing(kind, id)),
        }
    }

    /// Several objects at once, each answered for on its own.
    ///
    /// One that is not there is not a failure of the request: the caller asked
    /// about several things and is told about each, which is why a page can
    /// draw the panels it found and say so about the one it did not.
    pub fn bulk_get(&self, asked: &[Value]) -> Result<Value, Failed> {
        let mut out = Vec::new();
        for one in asked {
            let kind = one.get("type").and_then(|v| v.as_str()).unwrap_or_default();
            let id = one.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            match self.get(kind, id) {
                Ok(found) => out.push(found),
                Err(e) => out.push(json!({
                    "id": id, "type": kind,
                    "error": {"statusCode": e.status, "error": reason(e.status), "message": e.message},
                })),
            }
        }
        Ok(json!({"saved_objects": out}))
    }

    /// Write a new object.
    pub fn create(&self, writing: Writing) -> Result<Value, Failed> {
        let id = writing.id.clone().unwrap_or_else(random_id);
        let document = document_id(&writing.kind, &id);
        let migration = writing
            .migration_version
            .clone()
            .or_else(|| self.migration_versions.get(&writing.kind).cloned())
            .unwrap_or_else(|| json!({}));
        let mut source = Map::new();
        source.insert(writing.kind.clone(), writing.attributes.clone());
        source.insert("type".into(), json!(writing.kind));
        source.insert("references".into(), writing.references.clone());
        if migration.as_object().is_some_and(|o| !o.is_empty()) {
            source.insert("migrationVersion".into(), migration);
        }
        source.insert("updated_at".into(), json!(super::now()));
        // `create` where the caller did not ask to overwrite, so that two
        // people making the same dashboard is a conflict rather than one of
        // them quietly winning
        let operation = match writing.overwrite {
            true => "_doc",
            false => "_create",
        };
        // `require_alias` so that a write can never be the thing that makes
        // the console's index: an index made that way is a plain one under the
        // alias's own name, and nothing can put an alias over it afterwards
        let path = format!("/{INDEX}/{operation}/{document}?refresh=wait_for&require_alias=true");
        let mut found = self.engine.call("PUT", &path, Some(&Value::Object(source.clone())))?;
        if refused_for_the_index(&found) {
            self.make_it_right()?;
            found = self.engine.call("PUT", &path, Some(&Value::Object(source.clone())))?;
        }
        if let Some(kind) = found.pointer("/error/type").and_then(|v| v.as_str()) {
            let reason = found
                .pointer("/error/reason")
                .and_then(|v| v.as_str())
                .unwrap_or("the object could not be written");
            // the engine's own words twice over, which is what the server
            // being replaced says: once as the message and once as the cause,
            // and the reason already begins with the document's name
            return Err(Failed {
                status: if kind == "version_conflict_engine_exception" { 409 } else { 400 },
                message: format!("{reason}: {kind}: [{kind}] Reason: {reason}"),
            });
        }
        let mut answer = shape_source(&Value::Object(source), &writing.kind, &id);
        answer["version"] = json!(version_of(&found));
        Ok(reorder(answer))
    }

    /// Change an object that is already there.
    ///
    /// A change is of the attributes it names and nothing else, so a page that
    /// knows about one field does not have to send back the ones it does not.
    pub fn update(
        &self,
        kind: &str,
        id: &str,
        attributes: &Value,
        references: Option<&Value>,
    ) -> Result<Value, Failed> {
        let document = document_id(kind, id);
        let mut change = Map::new();
        change.insert(kind.to_string(), attributes.clone());
        change.insert("updated_at".into(), json!(super::now()));
        if let Some(references) = references {
            change.insert("references".into(), references.clone());
        }
        let found = self.engine.call(
            "POST",
            &format!("/{INDEX}/_update/{document}?refresh=wait_for"),
            Some(&json!({"doc": change})),
        )?;
        if found.pointer("/error").is_some() {
            return Err(missing(kind, id));
        }
        let mut answer = json!({
            "id": id,
            "type": kind,
            "updated_at": change.get("updated_at"),
            "version": version_of(&found),
            "namespaces": ["default"],
            "attributes": attributes,
        });
        if let Some(references) = references {
            answer["references"] = references.clone();
        }
        Ok(answer)
    }

    /// Forget an object.
    pub fn delete(&self, kind: &str, id: &str) -> Result<Value, Failed> {
        let path = format!("/{INDEX}/_doc/{}?refresh=wait_for", document_id(kind, id));
        let found = self.engine.call("DELETE", &path, None)?;
        match found.get("result").and_then(|v| v.as_str()) {
            Some("deleted") => Ok(json!({})),
            _ => Err(missing(kind, id)),
        }
    }
}

/// The id a saved object has as a document.
///
/// The type is part of it so that a dashboard and a search may both be called
/// `sales` without being the same thing.
pub fn document_id(kind: &str, id: &str) -> String {
    let id = format!("{kind}:{id}");
    id.replace('%', "%25").replace('/', "%2F").replace('#', "%23").replace('?', "%3F")
}

/// A hit, as the API describes an object.
pub fn shape(hit: &Value, kind: &str, id: &str) -> Value {
    let source = hit.get("_source").cloned().unwrap_or_else(|| json!({}));
    let mut out = shape_source(&source, kind, id);
    out["version"] = json!(version_of(hit));
    out
}

fn shape_source(source: &Value, kind: &str, id: &str) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), json!(id));
    out.insert("type".into(), json!(kind));
    out.insert("namespaces".into(), json!(["default"]));
    if let Some(at) = source.get("updated_at") {
        out.insert("updated_at".into(), at.clone());
    }
    // the version is filled in by the caller, which has the hit; it stands
    // here so that the keys come out in the order the server being replaced
    // writes them
    out.insert("version".into(), Value::Null);
    out.insert("attributes".into(), source.get(kind).cloned().unwrap_or_else(|| json!({})));
    out.insert("references".into(), source.get("references").cloned().unwrap_or_else(|| json!([])));
    if let Some(version) = source.get("migrationVersion") {
        out.insert("migrationVersion".into(), version.clone());
    }
    Value::Object(out)
}

/// What the console calls a document's version: where it stood, written the
/// way the front end sends it back.
///
/// Two numbers -- the sequence number and the term -- because either alone can
/// repeat after a primary changes, and a change applied to the wrong one is a
/// change applied to somebody else's edit.
pub fn version_of(found: &Value) -> String {
    let seq = found.get("_seq_no").and_then(|v| v.as_u64()).unwrap_or(0);
    let term = found.get("_primary_term").and_then(|v| v.as_u64()).unwrap_or(0);
    base64::engine::general_purpose::STANDARD.encode(format!("[{seq},{term}]"))
}

/// Whether a write was refused because the console's index is not where it
/// should be, rather than because of anything the caller did.
fn refused_for_the_index(found: &Value) -> bool {
    matches!(
        found.pointer("/error/type").and_then(|v| v.as_str()),
        Some("index_not_found_exception" | "invalid_alias_name_exception")
    ) || found
        .pointer("/error/reason")
        .and_then(|v| v.as_str())
        .is_some_and(|r| r.contains("does not point to an alias"))
}

/// An object that is not there, in the words the front end shows.
pub fn missing(kind: &str, id: &str) -> Failed {
    Failed { status: 404, message: format!("Saved object [{kind}/{id}] not found") }
}

fn reason(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Internal Server Error",
    }
}

/// A name for an object nobody named.
fn random_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mut seed = now as u64 ^ ((std::process::id() as u64) << 32);
    let mut hex = String::new();
    for _ in 0..32 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        hex.push(char::from_digit(((seed >> 33) % 16) as u32, 16).unwrap_or('0'));
    }
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

impl Default for Looking {
    fn default() -> Looking {
        Looking {
            types: Vec::new(),
            search: None,
            search_fields: Vec::new(),
            fields: Vec::new(),
            page: 1,
            per_page: 20,
            sort_field: None,
            sort_order: None,
            has_reference: None,
            default_search_operator: "OR".into(),
        }
    }
}

impl Saved<'_> {
    /// The objects a page is looking for.
    ///
    /// A type nobody has written anything of is not an error -- it is a page
    /// with nothing on it yet, which is what a console looks like the first
    /// time somebody opens it.
    pub fn find(&self, looking: &Looking) -> Result<Value, Failed> {
        let body = self.query_for(looking);
        let found = self.engine.call("POST", &format!("/{INDEX}/_search"), Some(&body))?;
        if let Some(reason) = found.pointer("/error/reason").and_then(|v| v.as_str()) {
            // an index that is not there yet holds nothing, which is the
            // answer rather than a failure
            if found.pointer("/error/type").and_then(|v| v.as_str())
                == Some("index_not_found_exception")
            {
                return Ok(empty(looking));
            }
            return Err(Failed { status: 400, message: reason.to_string() });
        }
        let hits: Vec<Value> =
            found.pointer("/hits/hits").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let total = found.pointer("/hits/total/value").and_then(|v| v.as_u64()).unwrap_or(0);
        let objects: Vec<Value> = hits
            .iter()
            .filter_map(|hit| {
                let source = hit.get("_source")?;
                let kind = source.get("type")?.as_str()?;
                let id = hit.get("_id")?.as_str()?.strip_prefix(&format!("{kind}:"))?;
                let mut one = shape(hit, kind, id);
                // a caller that named the fields it wants is drawing a list
                // and does not need the rest of every object
                if !looking.fields.is_empty()
                    && let Some(attributes) = one["attributes"].as_object()
                {
                    let kept: Map<String, Value> = attributes
                        .iter()
                        .filter(|(k, _)| looking.fields.contains(k))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    one["attributes"] = Value::Object(kept);
                }
                one["score"] = hit.get("_score").cloned().unwrap_or(json!(0));
                Some(reorder(one))
            })
            .collect();
        Ok(json!({
            "page": looking.page,
            "per_page": looking.per_page,
            "total": total,
            "saved_objects": objects,
        }))
    }

    /// The search a request stands for.
    fn query_for(&self, looking: &Looking) -> Value {
        let mut must: Vec<Value> = Vec::new();
        if !looking.types.is_empty() {
            must.push(json!({"terms": {"type": looking.types}}));
        }
        if let Some(text) = &looking.search {
            // a search names the fields it searches as `title^3` and the like,
            // and where it names none it searches the attributes of the types
            // it asked about
            let fields: Vec<String> = match looking.search_fields.is_empty() {
                false => looking
                    .search_fields
                    .iter()
                    .flat_map(|field| {
                        let (name, boost) = match field.split_once('^') {
                            Some((name, boost)) => (name, format!("^{boost}")),
                            None => (field.as_str(), String::new()),
                        };
                        looking.types.iter().map(move |kind| format!("{kind}.{name}{boost}"))
                    })
                    .collect(),
                true => vec!["*".to_string()],
            };
            must.push(json!({"simple_query_string": {
                "query": text,
                "fields": fields,
                "default_operator": looking.default_search_operator,
            }}));
        }
        if let Some(reference) = &looking.has_reference {
            must.push(json!({"nested": {
                "path": "references",
                "query": {"bool": {"must": [
                    {"term": {"references.type": reference.get("type")}},
                    {"term": {"references.id": reference.get("id")}},
                ]}},
            }}));
        }
        let mut body = json!({
            "query": {"bool": {"must": must}},
            "size": looking.per_page,
            "from": looking.page.saturating_sub(1) * looking.per_page,
            "track_total_hits": true,
        });
        if let Some(field) = &looking.sort_field {
            // a sort is on a field of the type asked for, because the same
            // name means different things under different types
            let on = match looking.types.len() {
                1 => format!("{}.{field}", looking.types[0]),
                _ => field.clone(),
            };
            let order = looking.sort_order.clone().unwrap_or_else(|| "desc".into());
            body["sort"] = json!([{on: {"order": order, "unmapped_type": "keyword"}}]);
        }
        body
    }
}

/// The answer to a search of an index that holds nothing yet.
fn empty(looking: &Looking) -> Value {
    json!({
        "page": looking.page,
        "per_page": looking.per_page,
        "total": 0,
        "saved_objects": [],
    })
}

///
/// The order of the keys is not something a reader should depend on -- the
/// suite compares whole objects and JavaScript does not care -- but writing
/// them the way the server being replaced writes them costs nothing and makes
/// a difference somebody notices a real difference.
fn reorder(one: Value) -> Value {
    let Value::Object(o) = &one else { return one };
    let mut out = Map::new();
    for key in [
        "type",
        "id",
        "attributes",
        "references",
        "migrationVersion",
        "updated_at",
        "version",
        "namespaces",
        "score",
    ] {
        if let Some(value) = o.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    for (key, value) in o {
        out.entry(key.clone()).or_insert_with(|| value.clone());
    }
    Value::Object(out)
}

/// What a caller asked to look for.
pub struct Looking {
    pub types: Vec<String>,
    pub search: Option<String>,
    pub search_fields: Vec<String>,
    pub fields: Vec<String>,
    pub page: u64,
    pub per_page: u64,
    pub sort_field: Option<String>,
    pub sort_order: Option<String>,
    pub has_reference: Option<Value>,
    pub default_search_operator: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_is_named_for_its_type_as_well_as_its_id() {
        // a dashboard and a search may both be called `sales`
        assert_eq!(document_id("dashboard", "sales"), "dashboard:sales");
        assert_ne!(document_id("search", "sales"), document_id("dashboard", "sales"));
    }

    #[test]
    fn an_id_that_would_be_a_path_of_its_own_is_not_one() {
        assert_eq!(document_id("index-pattern", "a/b"), "index-pattern:a%2Fb");
        assert!(!document_id("x", "a?b#c").contains(['?', '#']));
    }

    #[test]
    fn a_version_is_where_the_document_stood() {
        let found = json!({"_seq_no": 1, "_primary_term": 1});
        assert_eq!(version_of(&found), "WzEsMV0=", "as the front end sends it back");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(version_of(&json!({"_seq_no": 3, "_primary_term": 2})))
            .expect("base64");
        assert_eq!(String::from_utf8_lossy(&decoded), "[3,2]");
    }

    #[test]
    fn an_object_is_described_by_its_type_and_what_is_under_it() {
        let hit = json!({
            "_seq_no": 5, "_primary_term": 1,
            "_source": {
                "type": "index-pattern",
                "index-pattern": {"title": "logs-*"},
                "references": [],
                "updated_at": "2026-01-01T00:00:00.000Z",
                "migrationVersion": {"index-pattern": "7.6.0"},
            },
        });
        let found = shape(&hit, "index-pattern", "ip1");
        assert_eq!(found["attributes"]["title"], "logs-*");
        assert_eq!(found["id"], "ip1");
        assert_eq!(found["namespaces"], json!(["default"]));
        assert_eq!(found["migrationVersion"]["index-pattern"], "7.6.0");
    }
}
