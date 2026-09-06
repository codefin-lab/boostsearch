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
    /// How many of each type there are, by type.
    ///
    /// The released server nests this under `type`; its own suite on the same
    /// branch expects it flat, and fails against it. The suite is the newer
    /// of the two, and a flat map is what the page reads.
    pub fn counts(&self, types: &[String], search: Option<&str>) -> Result<Value, Failed> {
        let mut counts = Map::new();
        for kind in types {
            // a search string is matched as a prefix on the title, which is
            // what the page's own search box does
            let looking = Looking {
                types: vec![kind.clone()],
                search: search.map(|s| format!("{}*", s.trim_end_matches('*'))),
                search_fields: vec!["title".into()],
                per_page: 0,
                ..Default::default()
            };
            let found = self.saved.find(&looking)?;
            counts.insert(kind.clone(), found.get("total").cloned().unwrap_or(json!(0)));
        }
        Ok(Value::Object(counts))
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
    management: &Management<'_>,
    types: &[String],
    objects: &[Value],
    include_references: bool,
    exclude_details: bool,
) -> Result<Vec<Value>, Failed> {
    let saved = &management.saved;
    // one or the other: a request that names both is asking two questions
    if !types.is_empty() && !objects.is_empty() {
        return Err(Failed {
            objects: None,
            error: None,
            attributes: None,
            status: 400,
            message: "Can't specify both \"types\" and \"objects\" properties when exporting"
                .into(),
        });
    }
    if let Some(bad) = types.iter().find(|t| !management.allowed.contains(t)) {
        return Err(Failed {
            objects: None,
            error: None,
            attributes: None,
            status: 400,
            message: format!("Trying to export non-exportable type(s): {bad}"),
        });
    }
    let bad: Vec<String> = objects
        .iter()
        .filter(|o| {
            let kind = o.get("type").and_then(|v| v.as_str()).unwrap_or_default();
            !management.allowed.iter().any(|t| t == kind)
        })
        .map(|o| {
            format!(
                "{}:{}",
                o.get("type").and_then(|v| v.as_str()).unwrap_or_default(),
                o.get("id").and_then(|v| v.as_str()).unwrap_or_default()
            )
        })
        .collect();
    if !bad.is_empty() {
        return Err(Failed {
            objects: None,
            error: None,
            attributes: None,
            status: 400,
            message: format!(
                "Trying to export object(s) with non-exportable types: {}",
                bad.join(", ")
            ),
        });
    }
    let mut found: Vec<Value> = Vec::new();
    if !objects.is_empty() {
        let got = saved.bulk_get(objects)?;
        // an object that is not there is not skipped: the reader asked for
        // it by name, and a file without it would not be the file they asked
        // for. Each one that is missing is named.
        let missing: Vec<Value> = got
            .get("saved_objects")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter(|one| one.get("error").is_some())
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(Failed::with_objects("Bad Request", missing));
        }
        for one in got.get("saved_objects").and_then(|v| v.as_array()).into_iter().flatten() {
            found.push(one.clone());
        }
    }
    if !types.is_empty() {
        let looking = Looking {
            types: types.to_vec(),
            // an export is of everything, not of a page of it -- up to the
            // most a file may hold, which is also as far as a search will
            // page; one over is known from the count and is a refusal rather
            // than a file with the last object quietly left out
            per_page: IMPORT_LIMIT as u64,
            ..Default::default()
        };
        // how many first, and only then which: ten thousand objects is a
        // body worth not fetching when the answer is going to be no
        let counting = Looking { per_page: 0, types: types.to_vec(), ..Default::default() };
        let total = saved.find(&counting)?.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
        if total as usize > IMPORT_LIMIT {
            return Err(Failed {
                objects: None,
                error: None,
                attributes: None,
                status: 400,
                message: format!("Can't export more than {IMPORT_LIMIT} objects"),
            });
        }
        let page = saved.find(&looking)?;
        for one in page.get("saved_objects").and_then(|v| v.as_array()).cloned().unwrap_or_default()
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
    // What an object points at comes before it in the file, so that an import
    // reading a line at a time has the index pattern before the visualization
    // that draws on it, and the visualization before the dashboard that shows
    // it. A file that is read back one line at a time and written as it goes
    // has to be in that order or the references dangle.
    let ordered = in_dependency_order(found);
    let mut lines: Vec<Value> = ordered.iter().map(exported).collect();
    if !exclude_details {
        lines.push(json!({
            "exportedCount": ordered.len(),
            "missingRefCount": missing.len(),
            "missingReferences": missing,
        }));
    }
    Ok(lines)
}

/// Objects sorted so that anything one points at comes before it.
///
/// A dashboard points at visualizations, a visualization at an index
/// pattern; the index pattern has to be first. Objects that point at nothing
/// keep the order they came in, and so does everything else where the order
/// does not matter.
fn in_dependency_order(objects: Vec<Value>) -> Vec<Value> {
    let key = |o: &Value| {
        format!(
            "{}:{}",
            o.get("type").and_then(|v| v.as_str()).unwrap_or_default(),
            o.get("id").and_then(|v| v.as_str()).unwrap_or_default()
        )
    };
    let present: std::collections::HashSet<String> = objects.iter().map(key).collect();
    let mut placed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<Value> = Vec::with_capacity(objects.len());
    let mut left: Vec<Value> = objects;
    // each pass places everything whose dependencies are all placed; an
    // object pointing at something not in the export does not wait for it
    while !left.is_empty() {
        let mut progressed = false;
        let mut still = Vec::new();
        for one in left {
            let ready =
                one.get("references").and_then(|v| v.as_array()).into_iter().flatten().all(|r| {
                    let k = format!(
                        "{}:{}",
                        r.get("type").and_then(|v| v.as_str()).unwrap_or_default(),
                        r.get("id").and_then(|v| v.as_str()).unwrap_or_default()
                    );
                    !present.contains(&k) || placed.contains(&k)
                });
            if ready {
                placed.insert(key(&one));
                out.push(one);
                progressed = true;
            } else {
                still.push(one);
            }
        }
        left = still;
        if !progressed {
            // a cycle: whatever is left goes out as it is, rather than never
            out.extend(left);
            break;
        }
    }
    out
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

/// What the reader said to do about one object that could not be imported
/// the first time.
#[derive(Default, Clone)]
pub struct Retry {
    pub overwrite: bool,
    pub destination: Option<String>,
    /// references to point somewhere else: (type, from, to)
    pub replace: Vec<(String, String, String)>,
}

/// The most an import may hold. Past it the answer is a 400 rather than a
/// long wait, because a file that size is a mistake far more often than a
/// plan.
pub const IMPORT_LIMIT: usize = 10_000;

/// Read an export back in.
///
/// The interesting case is a conflict: an object whose id is already taken.
/// Overwriting is one answer and refusing is another, and which is right is
/// the reader's to say -- so by default it is refused and reported, and the
/// reader is asked. That is what `_resolve_import_errors` is for: the same
/// file again, with an answer for each object that was refused.
///
/// Nothing is written until everything has been checked, so a file with one
/// bad object in it does not half-import.
pub fn import(
    management: &Management<'_>,
    lines: &str,
    overwrite: bool,
    retries: Option<&std::collections::BTreeMap<(String, String), Retry>>,
) -> Result<Value, Failed> {
    let objects: Vec<Value> = lines
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        // the line that says what the file held is not an object in it
        .filter(|one| one.get("exportedCount").is_none())
        .collect();
    if objects.len() > IMPORT_LIMIT {
        return Err(Failed {
            objects: None,
            error: None,
            attributes: None,
            status: 400,
            message: format!("Can't import more than {IMPORT_LIMIT} objects"),
        });
    }
    let saved = &management.saved;
    let key = |one: &Value| {
        (
            one.get("type").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            one.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        )
    };
    let title_of = |one: &Value| one.pointer("/attributes/title").cloned().unwrap_or(Value::Null);
    let described = |one: &Value, error: Value| {
        let (kind, id) = key(one);
        let mut meta = json!({"title": title_of(one)});
        if let Some(icon) = management.meta.get(&kind).and_then(|m| m.get("icon")) {
            meta["icon"] = icon.clone();
        }
        json!({"id": id, "type": kind, "title": title_of(one), "meta": meta, "error": error})
    };

    // with an answer sheet, only the objects on it are considered at all
    let mut considered: Vec<(Value, Retry)> = Vec::new();
    for one in objects {
        match retries {
            Some(sheet) => {
                if let Some(retry) = sheet.get(&key(&one)) {
                    considered.push((one, retry.clone()));
                }
            }
            None => considered.push((one, Retry { overwrite, ..Default::default() })),
        }
    }

    // an object in the file counts as present for anything else in the file
    // that points at it: the index pattern is a line above the visualization
    // that draws on it, and the whole point of the file is that they travel
    // together
    let in_file: std::collections::HashSet<(String, String)> =
        considered.iter().map(|(one, _)| key(one)).collect();
    let mut errors: Vec<Value> = Vec::new();
    let mut to_write: Vec<(Value, Retry)> = Vec::new();
    for (one, retry) in considered {
        let (kind, _) = key(&one);
        if !management.allowed.contains(&kind) {
            errors.push(described(&one, json!({"type": "unsupported_type"})));
            continue;
        }
        // an export from an older console is in an older shape, and it is
        // brought up to date the way anything read off disk is: a file that
        // does not say what it has been through has been through nothing
        let mut one = one;
        if one.get("migrationVersion").is_none() {
            one["migrationVersion"] = json!({});
        }
        let mut one = match super::migrations::migrate(one) {
            Ok(migrated) => migrated,
            Err(message) => {
                errors.push(json!({"type": kind,
                                   "error": {"type": "unknown", "message": message, "statusCode": 422}}));
                continue;
            }
        };
        // references the reader asked to point somewhere else
        if !retry.replace.is_empty()
            && let Some(refs) = one.get_mut("references").and_then(|v| v.as_array_mut())
        {
            for r in refs.iter_mut() {
                for (rt, from, to) in &retry.replace {
                    if r.get("type").and_then(|v| v.as_str()) == Some(rt)
                        && r.get("id").and_then(|v| v.as_str()) == Some(from)
                    {
                        r["id"] = json!(to);
                    }
                }
            }
        }
        // an object that draws on an index pattern or a search that is not
        // there would import and never draw; better to say so now
        let mut missing = Vec::new();
        for r in one.get("references").and_then(|v| v.as_array()).into_iter().flatten() {
            let rt = r.get("type").and_then(|v| v.as_str()).unwrap_or_default();
            let rid = r.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            if !matches!(rt, "index-pattern" | "search") {
                continue;
            }
            let (rt, rid) = (rt.to_string(), rid.to_string());
            if in_file.contains(&(rt.clone(), rid.clone())) {
                continue;
            }
            if saved.get(&rt, &rid).is_err() {
                missing.push(json!({"type": rt, "id": rid}));
            }
        }
        if !missing.is_empty() {
            errors.push(described(
                &one,
                json!({"type": "missing_references", "references": missing}),
            ));
            continue;
        }
        to_write.push((one, retry));
    }

    // one write for the file rather than one per object: a file is as long
    // as the caller's export was, and each object waits for a refresh
    let writings: Vec<Writing> = to_write
        .iter()
        .map(|(one, retry)| {
            let (kind, id) = key(one);
            Writing {
                kind,
                id: Some(retry.destination.clone().unwrap_or(id)),
                attributes: one.get("attributes").cloned().unwrap_or_else(|| json!({})),
                references: one.get("references").cloned().unwrap_or_else(|| json!([])),
                migration_version: one.get("migrationVersion").cloned(),
                overwrite: retry.overwrite,
            }
        })
        .collect();
    let mut written: Vec<Value> = Vec::new();
    for ((one, retry), answer) in to_write.iter().zip(saved.bulk_create(writings)?) {
        let (_, id) = key(one);
        match answer {
            Ok(found) => {
                let mut result = described(one, Value::Null);
                if let Some(o) = result.as_object_mut() {
                    o.remove("error");
                    o.remove("title");
                }
                result["id"] = found.get("id").cloned().unwrap_or(json!(id));
                if retry.overwrite {
                    result["overwrite"] = json!(true);
                }
                if let Some(dest) = &retry.destination {
                    result["destinationId"] = json!(dest);
                }
                written.push(result);
            }
            Err(e) if e.status == 409 => errors.push(described(one, json!({"type": "conflict"}))),
            Err(e) => errors.push(described(
                one,
                json!({"type": "unknown", "message": e.message, "statusCode": e.status}),
            )),
        }
    }
    let mut out = Map::new();
    out.insert("success".into(), json!(errors.is_empty()));
    out.insert("successCount".into(), json!(written.len()));
    if !written.is_empty() {
        out.insert("successResults".into(), Value::Array(written));
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
