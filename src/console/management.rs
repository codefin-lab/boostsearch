//! Export, import, and the routes the Saved Objects page calls.
//!
//! Exporting is what makes a dashboard portable. An object names the ones it
//! points at by *reference* -- a name, a type and an id -- rather than by id
//! alone, so an export can carry a dashboard and everything it draws, and an
//! import somewhere else can renumber all of it and still have it draw.

use serde_json::{Map, Value, json};

use super::engine::{Engine, Failed, INDEX};
use super::saved::{Looking, Saved, Writing, document_id, shape};

/// The page that lists everything somebody has saved.
pub struct Management<'a> {
    pub saved: Saved<'a>,
    pub engine: &'a Engine,
    /// what the page shows for each type: the icon, where to edit it
    pub meta: &'a std::collections::BTreeMap<String, Value>,
    pub allowed: &'a [String],
}

impl<'a> Management<'a> {
    /// How many of each type there are.
    pub fn counts(&self, types: &[String], search: Option<&str>) -> Result<Value, Failed> {
        let mut counts = Map::new();
        for kind in types {
            let looking = Looking {
                types: vec![kind.clone()],
                search: search.map(String::from),
                search_fields: vec!["title".into()],
                per_page: 0,
                ..Default::default()
            };
            let found = self.saved.find(&looking)?;
            counts.insert(kind.clone(), found.get("total").cloned().unwrap_or(json!(0)));
        }
        Ok(json!({"type": counts}))
    }

    /// The objects the page lists, each with what to draw beside it.
    pub fn find(&self, looking: &Looking) -> Result<Value, Failed> {
        let mut found = self.saved.find(looking)?;
        let Some(objects) = found.get_mut("saved_objects").and_then(|v| v.as_array_mut()) else {
            return Ok(found);
        };
        for one in objects.iter_mut() {
            let kind = one.get("type").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let id = one.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let title = one.pointer("/attributes/title").and_then(|v| v.as_str()).map(String::from);
            one["meta"] = self.meta_for(&kind, &id, title);
        }
        Ok(found)
    }

    /// What the page draws beside an object.
    fn meta_for(&self, kind: &str, id: &str, title: Option<String>) -> Value {
        let mut meta = self.meta.get(kind).cloned().unwrap_or_else(|| json!({}));
        // the id is what makes each of these URLs that object's own
        if let Some(url) = meta.get("editUrl").and_then(|v| v.as_str()) {
            meta["editUrl"] = json!(url.replace("{id}", id));
        }
        if let Some(path) = meta.pointer("/inAppUrl/path").and_then(|v| v.as_str()) {
            let filled = path.replace("{id}", id);
            meta["inAppUrl"]["path"] = json!(filled);
        }
        if let Some(title) = title {
            let Value::Object(o) = &mut meta else { return meta };
            // the title goes after the icon, which is where the page's own
            // server puts it
            let mut ordered = Map::new();
            if let Some(icon) = o.remove("icon") {
                ordered.insert("icon".into(), icon);
            }
            ordered.insert("title".into(), json!(title));
            ordered.extend(o.clone());
            return Value::Object(ordered);
        }
        meta
    }

    /// One object, as the management page fetches it.
    pub fn one(&self, kind: &str, id: &str) -> Result<Value, Failed> {
        let found = self.saved.get(kind, id)?;
        let title = found.pointer("/attributes/title").and_then(|v| v.as_str()).map(String::from);
        let mut out = found.clone();
        out["meta"] = self.meta_for(kind, id, title);
        Ok(out)
    }

    /// What points at an object, and what it points at.
    ///
    /// Both directions, because the page asks the same question either way:
    /// deleting an index pattern matters because of what would stop drawing,
    /// and that is the things pointing at it rather than the things it points
    /// at.
    pub fn relationships(
        &self,
        kind: &str,
        id: &str,
        types: &[String],
        size: u64,
    ) -> Result<Value, Failed> {
        let object = self.saved.get(kind, id)?;
        let mut out = Vec::new();
        // what it points at
        for reference in object.get("references").and_then(|v| v.as_array()).into_iter().flatten() {
            let to = reference.get("type").and_then(|v| v.as_str()).unwrap_or_default();
            let to_id = reference.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            if !types.is_empty() && !types.iter().any(|t| t == to) {
                continue;
            }
            if let Ok(found) = self.one(to, to_id) {
                let mut one = found;
                one["relationship"] = json!("child");
                out.push(one);
            }
        }
        // what points at it
        let pointing = Looking {
            types: types.to_vec(),
            has_reference: Some(json!({"type": kind, "id": id})),
            per_page: size,
            ..Default::default()
        };
        for one in self
            .find(&pointing)?
            .get("saved_objects")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let mut one = one;
            one["relationship"] = json!("parent");
            out.push(one);
        }
        Ok(Value::Array(out))
    }

    /// The types the page may show.
    pub fn allowed_types(&self) -> Value {
        json!({"types": self.allowed})
    }
}

/// Everything asked for, one object to a line, and a line saying what was
/// exported.
///
/// A line at a time rather than one document, because an export is read back
/// a line at a time -- an import that failed halfway has still written what
/// it read, and the file says what was in it.
pub fn export(
    saved: &Saved<'_>,
    types: &[String],
    objects: &[Value],
    include_references: bool,
    exclude_details: bool,
) -> Result<String, Failed> {
    let mut found: Vec<Value> = Vec::new();
    if !objects.is_empty() {
        let got = saved.bulk_get(objects)?;
        for one in got.get("saved_objects").and_then(|v| v.as_array()).into_iter().flatten() {
            if let Some(error) = one.get("error") {
                let kind = one.get("type").and_then(|v| v.as_str()).unwrap_or_default();
                let id = one.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                let _ = error;
                return Err(Failed {
                    status: 400,
                    message: format!(
                        "Error fetching objects to export: Saved object [{kind}/{id}] not found"
                    ),
                });
            }
            found.push(one.clone());
        }
    }
    if !types.is_empty() {
        let looking = Looking {
            types: types.to_vec(),
            // an export is of everything, not of a page of it
            per_page: 10_000,
            ..Default::default()
        };
        for one in saved
            .find(&looking)?
            .get("saved_objects")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            found.push(one);
        }
    }
    // what the objects point at, so that the file draws where it lands
    let mut missing = Vec::new();
    if include_references {
        let mut queue: Vec<Value> = found.clone();
        while let Some(one) = queue.pop() {
            for reference in one.get("references").and_then(|v| v.as_array()).into_iter().flatten()
            {
                let kind = reference.get("type").and_then(|v| v.as_str()).unwrap_or_default();
                let id = reference.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                let already = found.iter().any(|o| {
                    o.get("type").and_then(|v| v.as_str()) == Some(kind)
                        && o.get("id").and_then(|v| v.as_str()) == Some(id)
                });
                if already {
                    continue;
                }
                match saved.get(kind, id) {
                    Ok(other) => {
                        queue.push(other.clone());
                        found.push(other);
                    }
                    // a reference to something that is gone is worth saying:
                    // the file will not draw, and the reader should know now
                    Err(_) => missing.push(json!({"type": kind, "id": id})),
                }
            }
        }
    }
    let mut out = String::new();
    for one in &found {
        out.push_str(&exported(one).to_string());
        out.push('\n');
    }
    if !exclude_details {
        out.push_str(
            &json!({
                "exportedCount": found.len(),
                "missingRefCount": missing.len(),
                "missingReferences": missing,
            })
            .to_string(),
        );
        out.push('\n');
    }
    Ok(out)
}

/// An object as an export file carries it: the keys in order, and nothing
/// that only made sense where it came from.
fn exported(one: &Value) -> Value {
    let mut out = Map::new();
    for key in
        ["attributes", "id", "migrationVersion", "references", "type", "updated_at", "version"]
    {
        if let Some(value) = one.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    Value::Object(out)
}

/// Read an export back in.
///
/// The interesting case is a conflict: an object whose id is already taken.
/// Overwriting is one answer and refusing is another, and which one is right
/// is the reader's to say -- so by default it is refused and reported, and
/// the reader is asked. That is what `_resolve_import_errors` is for.
pub fn import(
    saved: &Saved<'_>,
    lines: &str,
    overwrite: bool,
    retries: &[Value],
) -> Result<Value, Failed> {
    let mut written = Vec::new();
    let mut errors = Vec::new();
    for line in lines.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(one) = serde_json::from_str::<Value>(line) else { continue };
        // the line that says what the file held is not an object in it
        if one.get("exportedCount").is_some() {
            continue;
        }
        let kind = one.get("type").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let id = one.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let retry = retries.iter().find(|r| {
            r.get("type").and_then(|v| v.as_str()) == Some(&kind)
                && r.get("id").and_then(|v| v.as_str()) == Some(&id)
        });
        // a reader who said what to do about this one said it here
        let overwrite = overwrite
            || retry.and_then(|r| r.get("overwrite")).and_then(|v| v.as_bool()).unwrap_or(false);
        let into = retry
            .and_then(|r| r.get("destinationId"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| id.clone());
        let writing = Writing {
            kind: kind.clone(),
            id: Some(into.clone()),
            attributes: one.get("attributes").cloned().unwrap_or_else(|| json!({})),
            references: one.get("references").cloned().unwrap_or_else(|| json!([])),
            migration_version: one.get("migrationVersion").cloned(),
            overwrite,
        };
        match saved.create(writing) {
            Ok(found) => written.push(found),
            Err(e) if e.status == 409 => errors.push(json!({
                "id": id, "type": kind,
                "title": one.pointer("/attributes/title"),
                "meta": {"title": one.pointer("/attributes/title")},
                "error": {"type": "conflict"},
            })),
            Err(e) => errors.push(json!({
                "id": id, "type": kind,
                "error": {"type": "unknown", "message": e.message, "statusCode": e.status},
            })),
        }
    }
    let mut out = Map::new();
    out.insert("success".into(), json!(errors.is_empty()));
    out.insert("successCount".into(), json!(written.len()));
    if !written.is_empty() {
        out.insert(
            "successResults".into(),
            Value::Array(
                written
                    .iter()
                    .map(|one| {
                        json!({
                            "type": one.get("type"),
                            "id": one.get("id"),
                            "meta": {"title": one.pointer("/attributes/title")},
                        })
                    })
                    .collect(),
            ),
        );
    }
    if !errors.is_empty() {
        out.insert("errors".into(), Value::Array(errors));
    }
    Ok(Value::Object(out))
}

/// Everything of some types, for a page that is about to show all of it.
pub fn scroll_export(saved: &Saved<'_>, types: &[String]) -> Result<Value, Failed> {
    let looking = Looking { types: types.to_vec(), per_page: 10_000, ..Default::default() };
    Ok(saved.find(&looking)?.get("saved_objects").cloned().unwrap_or_else(|| json!([])))
}

/// Every object of some types, counted without fetching any of them.
pub fn count_of(engine: &Engine, kind: &str) -> Result<u64, Failed> {
    let found = engine.call(
        "POST",
        &format!("/{INDEX}/_count"),
        Some(&json!({"query": {"term": {"type": kind}}})),
    )?;
    Ok(found.get("count").and_then(|v| v.as_u64()).unwrap_or(0))
}

/// The document an object is, for anything that wants it whole.
pub fn raw(engine: &Engine, kind: &str, id: &str) -> Result<Value, Failed> {
    let found = engine.call("GET", &format!("/{INDEX}/_doc/{}", document_id(kind, id)), None)?;
    Ok(shape(&found, kind, id))
}
