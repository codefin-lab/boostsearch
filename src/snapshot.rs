//! Snapshots that copy what they say they copy.
//!
//! A snapshot here is a directory of documents rather than a copy of the
//! index's own files: one file per index holding its mapping and settings, and
//! one per primary shard holding that shard's documents as they were written,
//! written by the node that holds the primary. It is slower to take and to
//! restore than copying segments would be, and it does not care which version
//! of the engine wrote it -- a restore re-indexes, so a snapshot outlives a
//! change of format.

pub mod blobs;
pub mod url;

/// Where a repository's files are, however they are reached.
///
/// A repository on a filesystem is a directory, a repository read over a URL
/// is not, and one in an object store is neither. Everything above them wants
/// the same four things: read this file, write this file, forget these files,
/// and tell me what snapshots are here.
pub enum Source {
    Dir(PathBuf),
    Url(String),
    Blobs(Box<dyn blobs::Store>),
}

impl Source {
    /// The source a registered repository stands for.
    pub fn of(repo: &Value) -> Option<Source> {
        if let Some(dir) = location(repo) {
            return Some(Source::Dir(dir));
        }
        if let Some(url) = url::url_of(repo) {
            return Some(Source::Url(url));
        }
        blobs::of(repo).map(Source::Blobs)
    }

    /// Whether anything may be written here.
    pub fn writable(&self) -> bool {
        !matches!(self, Source::Url(_))
    }

    pub fn read(&self, relative: &str) -> Option<Vec<u8>> {
        // the same rule the write path has had: a name that climbs out of the
        // repository is not a file of the repository. A snapshot name is part
        // of this path, and a caller writes the snapshot name.
        if climbs(relative) {
            return None;
        }
        match self {
            Source::Dir(dir) => std::fs::read(dir.join(relative)).ok(),
            Source::Url(url) => url::fetch(url, relative),
            Source::Blobs(store) => store.get(relative),
        }
    }

    pub fn write(&self, relative: &str, bytes: &[u8]) -> std::io::Result<()> {
        if climbs(relative) {
            return Err(std::io::Error::other(format!(
                "[{relative}] is not a path inside the repository"
            )));
        }
        match self {
            Source::Dir(dir) => {
                let path = dir.join(relative);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                // a reader may be another node, or this node's own URL view
                // of the same directory: a file it catches half-written is a
                // snapshot that reads as empty or as nonsense
                crate::store::write_atomic(&path, bytes)
            }
            Source::Url(_) => Err(std::io::Error::other("this repository is read-only")),
            Source::Blobs(store) => store.put(relative, bytes),
        }
    }

    /// Forget everything a snapshot left behind.
    pub fn remove_prefix(&self, prefix: &str) {
        if climbs(prefix) || prefix.trim_matches('/').is_empty() {
            return;
        }
        match self {
            Source::Dir(dir) => {
                let _ = std::fs::remove_dir_all(dir.join(prefix));
            }
            Source::Url(_) => {}
            Source::Blobs(store) => store.delete_prefix(prefix),
        }
    }

    /// The snapshots this source holds, and what each of them recorded.
    ///
    /// A directory is looked at; anything else is asked, through the index
    /// its writer left behind.
    pub fn records(&self) -> Vec<(String, Value)> {
        match self {
            Source::Dir(dir) => read_records(dir),
            Source::Url(url) => url::read_records(url),
            // an object store can be asked what is in it, so it is asked
            // rather than being taken at the word of an index it wrote
            // earlier -- which is also what keeps that index honest
            Source::Blobs(store) => store
                .list("")
                .into_iter()
                .filter_map(|name| Some(name.strip_suffix("/snapshot.json")?.to_string()))
                .filter(|name| !name.contains('/'))
                .filter_map(|name| {
                    let raw = self.read(&format!("{name}/snapshot.json"))?;
                    let record = serde_json::from_slice::<Value>(&raw).ok()?;
                    Some((name, record))
                })
                .collect(),
        }
    }

    /// Write down what this source now holds, for a reader that cannot look.
    pub fn write_index(&self) {
        let names: Vec<String> = match self {
            Source::Dir(dir) => read_records(dir).into_iter().map(|(n, _)| n).collect(),
            _ => self.records().into_iter().map(|(n, _)| n).collect(),
        };
        let _ = self.write("index.json", json!({"snapshots": names}).to_string().as_bytes());
    }
}

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use velocore::TantivyDocument;
use velocore::schema::document::Value as _;

use crate::store::{IdxState, Store};

/// Where repositories are allowed to live.
///
/// OpenSearch calls this `path.repo`, and a relative location is resolved
/// under it. Without one named, it sits beside the data a server was given, so
/// a location a client makes up cannot land anywhere it likes -- least of all
/// in whatever directory the process happens to have been started from.
/// Whether a relative path leaves the directory it is joined to: a
/// `..` component, or an absolute path, which `join` would take whole.
fn climbs(relative: &str) -> bool {
    let path = std::path::Path::new(relative);
    path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir))
}

pub fn repo_root() -> PathBuf {
    if let Ok(dir) = std::env::var("VELOSEARCH_PATH_REPO")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    match std::env::var("VELOSEARCH_DATA") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("repo"),
        _ => std::env::temp_dir().join("velosearch-repo"),
    }
}

/// A path with `.` and `..` taken out, without asking the filesystem: a
/// location that does not exist yet still has to be judged.
pub(crate) fn tidy(path: &std::path::Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Where a repository keeps its snapshots, if it is one we can write to.
///
/// Only `fs` repositories have somewhere to put anything; the rest are
/// registered and answered for, and hold nothing.
pub fn location(repo: &Value) -> Option<PathBuf> {
    if repo.get("type").and_then(|t| t.as_str()) != Some("fs") {
        return None;
    }
    let named =
        repo.pointer("/settings/location").and_then(|v| v.as_str()).filter(|s| !s.is_empty())?;
    let path = PathBuf::from(named);
    if path.is_absolute() {
        // an absolute location is allowed where `path.repo` allows it and
        // nowhere else: a repository at `/` is every file the process can
        // reach, and a delete of a snapshot in it is a delete of anything
        let root = repo_root();
        // `starts_with` compares components and does not know what `..`
        // means, so `<path.repo>/../../anywhere` "starts with" the root: the
        // path is resolved before it is judged, and a location that cannot
        // be resolved is judged on the components it would have had
        let cleaned = tidy(&path);
        let cleaned_root = tidy(&root);
        let inside = cleaned.starts_with(&cleaned_root);
        return inside.then_some(cleaned);
    }
    // a relative location is a name, not a path: nothing it contains may climb
    // out of the root repositories live under
    let mut out = repo_root();
    for part in path.components() {
        match part {
            std::path::Component::Normal(p) => out.push(p),
            _ => return None,
        }
    }
    Some(out)
}

/// The file one shard's documents are kept in, inside its index's directory.
fn shard_file(shard: u32) -> String {
    format!("shard-{shard}.ndjson")
}

pub(crate) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Write the documents of some primary shards of one index, as this node
/// holds them, and say what was written.
///
/// A snapshot of a cluster is written shard by shard by the node holding
/// each primary: the node a snapshot request reaches may hold none of them,
/// and writing what it happened to hold made a snapshot that said SUCCESS
/// and was missing every index kept elsewhere. What comes back is how the
/// index was made and, for each shard, how many documents its file holds and
/// its digest -- which the node coordinating the snapshot writes into the
/// index's description once every shard has answered.
pub fn write_shards(
    store: &Store,
    to: &Source,
    snapshot: &str,
    index: &str,
    shards: &[u32],
) -> Result<Value, String> {
    let Some(st) = store.get(index) else {
        return Err(format!("no copy of index [{index}] on this node"));
    };
    let started = now_millis();
    // a snapshot is of what has been written, so what is waiting to be
    // written is committed first
    let _ = st.write().refresh();
    let g = st.read();
    let count = g.shard_count().max(1) as u32;
    if let Some(bad) = shards.iter().find(|s| **s >= count) {
        return Err(format!("index [{index}] has no shard [{bad}]"));
    }
    let mut files: BTreeMap<u32, (Vec<u8>, usize)> =
        shards.iter().map(|s| (*s, (Vec::new(), 0))).collect();
    dump(&g, |shard, line| {
        if let Some((bytes, n)) = files.get_mut(&shard) {
            bytes.extend_from_slice(line);
            bytes.push(b'\n');
            *n += 1;
        }
    });
    let within = format!("{snapshot}/{}", crate::store::dir_name(index));
    let mut written = serde_json::Map::new();
    for (shard, (bytes, n)) in files {
        to.write(&format!("{within}/{}", shard_file(shard)), &bytes)
            .map_err(|e| format!("could not write shard [{shard}] of [{index}]: {e}"))?;
        written.insert(
            shard.to_string(),
            json!({
                "count": n,
                "sha256": sha256_hex(&bytes),
                "size_in_bytes": bytes.len(),
                "start_time_in_millis": started,
                "time_in_millis": now_millis().saturating_sub(started),
            }),
        );
    }
    Ok(json!({
        "meta": {
            "name": index,
            "mappings": g.mapping.raw,
            "settings": g.settings,
            "aliases": g.aliases,
            "number_of_shards": count,
        },
        "shards": written,
    }))
}

/// Write down what an index in a snapshot is: how it was made, and what
/// each of its shards' files must be when it is read back. Written after the
/// shards' files, so a description is only ever of files that are there.
pub fn write_index_meta(
    to: &Source,
    snapshot: &str,
    index: &str,
    meta: &Value,
    shards: &serde_json::Map<String, Value>,
    failed: &serde_json::Map<String, Value>,
) -> std::io::Result<()> {
    let mut meta = meta.clone();
    meta["shards"] = Value::Object(shards.clone());
    meta["failed_shards"] = Value::Object(failed.clone());
    let within = format!("{snapshot}/{}", crate::store::dir_name(index));
    to.write(&format!("{within}/meta.json"), meta.to_string().as_bytes())
}

/// Write a whole snapshot of indices this node holds every shard of.
pub fn write_local(
    store: &Store,
    to: &Source,
    name: &str,
    indices: &[String],
    record: &Value,
) -> Result<(), String> {
    for index in indices {
        let Some(count) = store.get(index).map(|st| st.read().shard_count().max(1) as u32) else {
            return Err(format!("no index [{index}] on this node"));
        };
        let shards: Vec<u32> = (0..count).collect();
        let written = write_shards(store, to, name, index, &shards)?;
        let done = written["shards"].as_object().cloned().unwrap_or_default();
        write_index_meta(to, name, index, &written["meta"], &done, &serde_json::Map::new())
            .map_err(|e| e.to_string())?;
    }
    write_record(to, name, record).map_err(|e| e.to_string())
}

/// Write a snapshot's record, which is what makes it a snapshot: it is
/// written last, once everything it describes is in place.
pub fn write_record(to: &Source, name: &str, record: &Value) -> std::io::Result<()> {
    to.write(&format!("{name}/snapshot.json"), record.to_string().as_bytes())?;
    to.write_index();
    Ok(())
}

/// What a snapshot keeps of the cluster besides its indices.
pub fn global_state(store: &Store) -> Value {
    json!({
        "customs": store.customs(),
        "persistent": store.cluster_settings().get("persistent").cloned().unwrap_or(json!({})),
    })
}

pub fn write_global(to: &Source, name: &str, global: &Value) -> std::io::Result<()> {
    to.write(&format!("{name}/global.json"), global.to_string().as_bytes())
}

/// The global state a snapshot kept, read and checked before any of it is
/// put in place.
pub fn read_global(from: &Source, name: &str) -> Result<Value, String> {
    let raw = from.read(&format!("{name}/global.json")).ok_or_else(|| {
        format!("[{name}] was taken with its global state, but the repository holds none of it")
    })?;
    let global: Value = serde_json::from_slice(&raw)
        .map_err(|e| format!("[{name}] cannot restore its global state: it is damaged ({e})"))?;
    if !global.get("customs").map(|c| c.is_object()).unwrap_or(false)
        || !global.get("persistent").map(|c| c.is_object()).unwrap_or(false)
    {
        return Err(format!("[{name}] cannot restore its global state: it is damaged"));
    }
    Ok(global)
}

/// Put a snapshot's global state in place, the way the reference does: the
/// legacy templates it holds are written over those of the same name and the
/// rest are kept; everything else it holds -- composable and component
/// templates, pipelines of both kinds, stored scripts and the persistent
/// settings -- replaces what is there, whole.
pub fn apply_global(store: &Store, global: &Value) {
    let customs = &global["customs"];
    let mut templates: serde_json::Map<String, Value> = store
        .get_templates()
        .into_iter()
        .filter(|(_, t)| t.get("__composable").is_none())
        .collect();
    if let Some(kept) = customs.get("templates").and_then(|t| t.as_object()) {
        for (name, t) in kept {
            templates.insert(name.clone(), t.clone());
        }
    }
    let mut next = customs.clone();
    next["templates"] = Value::Object(templates);
    store.replace_customs(&next);
    store.replace_persistent_settings(&global["persistent"]);
}

/// Copy what one snapshot holds into another, without reading it back
/// through an index.
///
/// A clone used to record a snapshot and write nothing: the record said
/// SUCCESS, the repository held no files under that name, and a restore from
/// it brought back an index with no documents -- or, before the check that
/// now stands in front of it, took the index that was there with it.
pub fn clone_into(
    from: &Source,
    to: &Source,
    name: &str,
    target: &str,
    indices: &[String],
    record: &Value,
) -> std::io::Result<()> {
    let missing =
        |what: String| std::io::Error::other(format!("[{name}] holds no [{what}] for index"));
    for index in indices {
        let dir = crate::store::dir_name(index);
        let Some(raw) = from.read(&format!("{name}/{dir}/meta.json")) else {
            return Err(missing(format!("meta.json] for index [{index}")));
        };
        let meta: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        // the files the description names: one per shard written, or the one
        // documents file a snapshot from before shards were kept apart holds
        let files: Vec<String> = match meta.get("shards").and_then(|s| s.as_object()) {
            Some(shards) => shards.keys().filter_map(|k| k.parse().ok()).map(shard_file).collect(),
            None => vec!["docs.ndjson".to_string()],
        };
        for file in files {
            let Some(bytes) = from.read(&format!("{name}/{dir}/{file}")) else {
                return Err(missing(format!("{file}] for index [{index}")));
            };
            to.write(&format!("{target}/{dir}/{file}"), &bytes)?;
        }
        to.write(&format!("{target}/{dir}/meta.json"), &raw)?;
    }
    if let Some(global) = from.read(&format!("{name}/global.json")) {
        to.write(&format!("{target}/global.json"), &global)?;
    }
    write_record(to, target, record)
}

/// What an index in a snapshot holds, shard by shard, for `_status`: `None`
/// for a shard that failed, and the bytes of every shard of a snapshot from
/// before shards were kept apart counted as one.
///
/// The size of the index's own description comes back beside it: it is a
/// file the snapshot wrote too, and the one an index with no documents has
/// anything in.
pub fn shard_stats(from: &Source, snapshot: &str, index: &str) -> Option<(Value, u64)> {
    let within = format!("{snapshot}/{}", crate::store::dir_name(index));
    let raw = from.read(&format!("{within}/meta.json"))?;
    let meta: Value = serde_json::from_slice(&raw).ok()?;
    let described = raw.len() as u64;
    if meta.get("shards").is_some() {
        return Some((meta, described));
    }
    let size = meta.pointer("/docs/sha256").and(from.read(&format!("{within}/docs.ndjson")));
    let mut out = meta.clone();
    out["shards"] = json!({"0": {"size_in_bytes": size.map(|b| b.len()).unwrap_or(0)}});
    Some((out, described))
}

/// The digest a snapshot records for a file it wrote.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// Every living document, as it was given to us, with the shard it is in.
fn dump(g: &IdxState, mut each: impl FnMut(u32, &[u8])) {
    let searcher = g.reader.searcher();
    for seg in searcher.segment_readers() {
        let Ok(store_reader) = seg.get_store_reader(1) else { continue };
        for doc_id in seg.doc_ids_alive() {
            let Ok(doc) = store_reader.get::<TantivyDocument>(doc_id) else { continue };
            let Some(id) = doc.get_first(g.fields.id).and_then(|v| v.as_str()) else { continue };
            let Some(raw) = doc.get_first(g.fields.source).and_then(|v| v.as_str()) else {
                continue;
            };
            let record = json!({
                "_id": id,
                "_routing": g.routing.get(id),
                "_source": raw,
            });
            each(g.shard_of_doc(id) as u32, record.to_string().as_bytes());
        }
    }
}

/// The snapshots a repository already holds.
///
/// Registering a repository is how a new process learns about them: the
/// records are on disk, not in a cluster state this server keeps.
pub fn read_records(dir: &Path) -> Vec<(String, Value)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path().join("snapshot.json")) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<Value>(&text) else { continue };
        let Some(name) = record.get("snapshot").and_then(|v| v.as_str()) else { continue };
        out.push((name.to_string(), record));
    }
    out
}

/// One document as a snapshot kept it: id, routing, source as sent, and that
/// source read.
type Kept = (String, Option<String>, String, Value);

/// An index as a restore will make it, read whole and checked against what
/// the snapshot recorded of it -- before anything in the cluster is touched.
pub struct Prepared {
    body: Value,
    records: Vec<Kept>,
}

/// Read what a snapshot holds of one index, and check all of it.
///
/// Nothing is made or replaced here. A restore reads every index it will
/// bring back through this first, so a snapshot that cannot be brought back
/// whole -- a file cut short, a line spoiled, a file missing, a shard that
/// was never written -- is found before any index is created or any closed
/// one is taken out of its way.
pub fn prepare(
    from: &Source,
    snapshot: &str,
    index: &str,
    request: &Value,
) -> Result<Prepared, String> {
    let within = format!("{snapshot}/{}", crate::store::dir_name(index));
    let meta: Value = from
        .read(&format!("{within}/meta.json"))
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .ok_or_else(|| format!("[{snapshot}] holds nothing for index [{index}]"))?;
    // What the request says about the index as it comes back: without its
    // aliases, where `include_aliases` is false -- an alias restored onto a
    // renamed copy stood over the original and the copy both, and every
    // search through it counted each document twice -- and with the
    // settings it names changed or left out.
    let aliases = if request.get("include_aliases").and_then(|v| v.as_bool()) == Some(false) {
        json!({})
    } else {
        meta.get("aliases").cloned().unwrap_or_else(|| json!({}))
    };
    let mut settings = meta.get("settings").cloned().unwrap_or_else(|| json!({}));
    let short = |key: &str| key.strip_prefix("index.").unwrap_or(key).to_string();
    let forget = |settings: &mut Value, key: &str| {
        let key = short(key);
        if let Some(o) = settings.as_object_mut() {
            o.remove(&key);
            o.remove(&format!("index.{key}"));
            if let Some(inner) = o.get_mut("index").and_then(|i| i.as_object_mut()) {
                inner.remove(&key);
            }
        }
    };
    let ignored: Vec<String> = match request.get("ignore_index_settings") {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        Some(Value::String(s)) => s.split(',').map(|s| s.trim().to_string()).collect(),
        _ => Vec::new(),
    };
    for key in &ignored {
        forget(&mut settings, key);
    }
    // A restored index is a new index, with an identity of its own: the
    // snapshot's settings carry the uuid of the one it was taken from, and
    // the index made from them took it. Everything that tells two incarnations
    // of a name apart by uuid then took the restored index for the closed one
    // it replaced, and it answered `index_closed_exception` until the next
    // publish. The reference gives a restored index a fresh uuid.
    forget(&mut settings, "uuid");
    if let Some(Value::Object(asked)) = request.get("index_settings") {
        let mut flat: Vec<(String, Value)> = Vec::new();
        for (k, v) in asked {
            match (k.as_str(), v) {
                ("index", Value::Object(inner)) => {
                    flat.extend(inner.iter().map(|(k, v)| (k.clone(), v.clone())))
                }
                _ => flat.push((short(k), v.clone())),
            }
        }
        for (k, v) in flat {
            forget(&mut settings, &k);
            if !settings.is_object() {
                settings = json!({});
            }
            settings[format!("index.{k}")] = v;
        }
    }
    // an index whose analysis cannot be built would be refused when it is
    // made, which for an index being replaced is after the old one is gone
    if let Some(complaint) = crate::analysis::Registry::complaint(&settings) {
        return Err(format!("[{snapshot}] cannot restore index [{index}]: {complaint}"));
    }
    let body = json!({
        "mappings": meta.get("mappings").cloned().unwrap_or_else(|| json!({})),
        "settings": settings,
        "aliases": aliases,
    });
    let damaged = |what: &str, why: String| {
        format!("[{snapshot}] cannot restore index [{index}]: {what} is damaged ({why})")
    };
    let mut records: Vec<Kept> = Vec::new();
    match meta.get("shards").and_then(|s| s.as_object()) {
        Some(shards) => {
            // A snapshot that could not write every shard says so, and an
            // index missing a shard is not brought back as though it were
            // whole -- unless the request asked for what there is.
            let partial = request.get("partial").and_then(|v| v.as_bool()).unwrap_or(false);
            let count = meta
                .get("number_of_shards")
                .and_then(|v| v.as_u64())
                .unwrap_or(shards.len() as u64) as u32;
            for shard in 0..count {
                let Some(recorded) = shards.get(&shard.to_string()) else {
                    if partial {
                        continue;
                    }
                    return Err(format!(
                        "[{snapshot}] index [{index}] wasn't fully snapshotted - cannot restore"
                    ));
                };
                let file = shard_file(shard);
                let Some(bytes) = from.read(&format!("{within}/{file}")) else {
                    return Err(format!(
                        "[{snapshot}] holds the mapping of index [{index}] but not the documents \
                         of its shard [{shard}]"
                    ));
                };
                let what = format!("the documents file of shard [{shard}]");
                read_documents(&bytes, recorded, &mut records)
                    .map_err(|why| damaged(&what, why))?;
            }
            if shards.is_empty() {
                return Err(format!(
                    "[{snapshot}] index [{index}] wasn't fully snapshotted - cannot restore"
                ));
            }
        }
        None => {
            // A snapshot from before shards were written apart: one file.
            // The documents are written by every snapshot, empty index or
            // not, so their absence is a repository that cannot be read
            // rather than an index that held nothing.
            let Some(bytes) = from.read(&format!("{within}/docs.ndjson")) else {
                return Err(format!(
                    "[{snapshot}] holds the mapping of index [{index}] but not its documents"
                ));
            };
            let recorded = meta.get("docs").cloned().unwrap_or(Value::Null);
            read_documents(&bytes, &recorded, &mut records)
                .map_err(|why| damaged("its documents file", why))?;
        }
    }
    Ok(Prepared { body, records })
}

/// Read one documents file, checked against the digest and count the
/// snapshot recorded for it where it recorded them, every line of it.
///
/// A damaged documents file used to restore as far as it could be read: a
/// line that did not parse was skipped, a file cut short ended early, and the
/// restore answered success with fewer documents than the snapshot took.
fn read_documents(bytes: &[u8], recorded: &Value, out: &mut Vec<Kept>) -> Result<(), String> {
    if let Some(want) = recorded.get("sha256").and_then(|v| v.as_str()) {
        let got = sha256_hex(bytes);
        if got != want {
            return Err(format!("sha256 {got}, the snapshot recorded {want}"));
        }
    }
    let mut read = 0u64;
    for (n, line) in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()).enumerate() {
        let parsed = std::str::from_utf8(line)
            .ok()
            .and_then(|l| serde_json::from_str::<Value>(l).ok())
            .and_then(|record| {
                let id = record.get("_id")?.as_str()?.to_string();
                let raw = record.get("_source")?.as_str()?.to_string();
                let source = serde_json::from_str::<Value>(&raw).ok()?;
                let routing = record.get("_routing").and_then(|v| v.as_str()).map(String::from);
                Some((id, routing, raw, source))
            });
        match parsed {
            Some(r) => out.push(r),
            None => return Err(format!("line {} cannot be read", n + 1)),
        }
        read += 1;
    }
    if let Some(want) = recorded.get("count").and_then(|v| v.as_u64())
        && read != want
    {
        return Err(format!("{read} documents, the snapshot recorded {want}"));
    }
    Ok(())
}

/// Make an index from what `prepare` read, under the name it is restored as.
///
/// Every document is written or the restore has failed: a document the index
/// would not take is an index that is not what the snapshot held. What this
/// made is the caller's to take away when it fails.
pub fn apply(store: &Store, prepared: Prepared, as_name: &str) -> Result<usize, String> {
    store.create(as_name, &prepared.body).map_err(|e| e.to_string())?;
    let Some(st) = store.get(as_name) else {
        return Err(format!("[{as_name}] could not be created"));
    };
    let mut count = 0usize;
    let mut g = st.write();
    for (id, routing, raw, source) in prepared.records {
        if let Some(r) = routing {
            g.routing.insert(id.clone(), r);
        }
        if let Err(e) =
            crate::api::write_doc_internal(&mut g, &id, source, "index", Some(raw), None)
        {
            return Err(format!(
                "[{as_name}] could not take document [{id}] back (answered {})",
                e.status()
            ));
        }
        count += 1;
    }
    g.restored = true;
    g.refresh().map_err(|e| format!("[{as_name}] could not be committed: {e}"))?;
    Ok(count)
}

/// Forget a snapshot, and everything it was keeping.
pub fn remove(from: &Source, name: &str) {
    from.remove_prefix(name);
    from.write_index();
}
