//! Snapshots that copy what they say they copy.
//!
//! A snapshot here is a directory of documents rather than a copy of the
//! index's own files: one file per index holding its mapping and settings, and
//! one holding its documents as they were written. It is slower to take and to
//! restore than copying segments would be, and it does not care which version
//! of the engine wrote it -- a restore re-indexes, so a snapshot outlives a
//! change of format.

pub mod url;

/// Where a repository's files are, however they are reached.
///
/// A repository on a filesystem is a directory and a repository read over a
/// URL is not, but everything above them wants the same two things: read this
/// file, and tell me what snapshots are here.
pub enum Source {
    Dir(PathBuf),
    Url(String),
}

impl Source {
    /// The source a registered repository stands for.
    pub fn of(repo: &Value) -> Option<Source> {
        if let Some(dir) = location(repo) {
            return Some(Source::Dir(dir));
        }
        url::url_of(repo).map(Source::Url)
    }

    pub fn read(&self, relative: &str) -> Option<Vec<u8>> {
        match self {
            Source::Dir(dir) => std::fs::read(dir.join(relative)).ok(),
            Source::Url(url) => url::fetch(url, relative),
        }
    }

    /// The snapshots this source holds, and what each of them recorded.
    pub fn records(&self) -> Vec<(String, Value)> {
        match self {
            Source::Dir(dir) => read_records(dir),
            Source::Url(url) => url::read_records(url),
        }
    }
}

use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};

use boostcore::TantivyDocument;
use boostcore::schema::document::Value as _;
use serde_json::{Value, json};

use crate::store::{IdxState, Store};

/// Where repositories are allowed to live.
///
/// OpenSearch calls this `path.repo`, and a relative location is resolved
/// under it. Without one named, it sits beside the data a server was given, so
/// a location a client makes up cannot land anywhere it likes -- least of all
/// in whatever directory the process happens to have been started from.
pub fn repo_root() -> PathBuf {
    if let Ok(dir) = std::env::var("BOOSTSEARCH_PATH_REPO")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    match std::env::var("BOOSTSEARCH_DATA") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("repo"),
        _ => std::env::temp_dir().join("boostsearch-repo"),
    }
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
        return Some(path);
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

/// Everything an index needs to come back: what it was, and what was in it.
pub fn write(
    store: &Store,
    dir: &Path,
    name: &str,
    indices: &[String],
    record: &Value,
) -> std::io::Result<()> {
    let root = dir.join(name);
    std::fs::create_dir_all(&root)?;
    for index in indices {
        let Some(st) = store.get(index) else { continue };
        // a snapshot is of what has been written, so what is waiting to be
        // written is committed first
        let _ = st.write().refresh();
        let g = st.read();
        let idx_dir = root.join(crate::store::dir_name(index));
        std::fs::create_dir_all(&idx_dir)?;
        std::fs::write(
            idx_dir.join("meta.json"),
            json!({
                "name": index,
                "mappings": g.mapping.raw,
                "settings": g.settings,
                "aliases": g.aliases,
            })
            .to_string(),
        )?;
        let mut out = BufWriter::new(std::fs::File::create(idx_dir.join("docs.ndjson"))?);
        dump(&g, &mut out)?;
        out.flush()?;
    }
    std::fs::write(root.join("snapshot.json"), record.to_string())?;
    write_index(dir);
    Ok(())
}

/// The names a repository holds, written where a reader that cannot list a
/// directory can find them.
///
/// A repository read over HTTP has no way to ask what is in it -- that is
/// what OpenSearch keeps its `index-N` blob for, and this is the same idea
/// under a plainer name. A repository on a filesystem does not need it and
/// writes it anyway, because a URL repository may be pointed at the same
/// directory later.
pub fn write_index(dir: &Path) {
    let names: Vec<String> = read_records(dir).into_iter().map(|(name, _)| name).collect();
    let _ = std::fs::write(
        dir.join("index.json"),
        json!({"snapshots": names}).to_string(),
    );
}

/// Write out every living document, as it was given to us.
fn dump(g: &IdxState, out: &mut impl Write) -> std::io::Result<()> {
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
            writeln!(out, "{record}")?;
        }
    }
    Ok(())
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

/// Put an index back the way the snapshot found it.
///
/// The mapping and settings are recreated first, then the documents are
/// written back through the ordinary path -- which is why a snapshot taken by
/// one version can be restored by another.
pub fn restore_index(
    store: &Store,
    from: &Source,
    snapshot: &str,
    index: &str,
    as_name: &str,
) -> Result<usize, String> {
    let within = format!("{snapshot}/{}", crate::store::dir_name(index));
    let meta: Value = from
        .read(&format!("{within}/meta.json"))
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .ok_or_else(|| format!("[{snapshot}] holds nothing for index [{index}]"))?;
    let body = json!({
        "mappings": meta.get("mappings").cloned().unwrap_or_else(|| json!({})),
        "settings": meta.get("settings").cloned().unwrap_or_else(|| json!({})),
        "aliases": meta.get("aliases").cloned().unwrap_or_else(|| json!({})),
    });
    store.create(as_name, &body).map_err(|e| e.to_string())?;
    let Some(st) = store.get(as_name) else {
        return Err(format!("[{as_name}] could not be created"));
    };
    let Some(docs) = from.read(&format!("{within}/docs.ndjson")) else {
        return Ok(0);
    };
    let mut count = 0usize;
    let mut g = st.write();
    for line in docs.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let Ok(line) = std::str::from_utf8(line) else { continue };
        let Ok(record) = serde_json::from_str::<Value>(line) else { continue };
        let Some(id) = record.get("_id").and_then(|v| v.as_str()) else { continue };
        let Some(raw) = record.get("_source").and_then(|v| v.as_str()) else { continue };
        let Ok(source) = serde_json::from_str::<Value>(raw) else { continue };
        if let Some(r) = record.get("_routing").and_then(|v| v.as_str()) {
            g.routing.insert(id.to_string(), r.to_string());
        }
        if crate::api::write_doc_versioned(&mut g, id, source, "index", Some(raw.to_string()), None)
            .is_ok()
        {
            count += 1;
        }
    }
    g.restored = true;
    let _ = g.refresh();
    Ok(count)
}

/// Forget a snapshot, and everything it was keeping.
pub fn remove(dir: &Path, name: &str) {
    let _ = std::fs::remove_dir_all(dir.join(name));
    write_index(dir);
}
