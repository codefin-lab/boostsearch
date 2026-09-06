//! A repository read over a URL, which is a repository nothing may be written
//! to.
//!
//! OpenSearch calls this `type: url`, and it exists so that a snapshot taken
//! on one cluster can be restored on another without giving the second one
//! write access to where the first one keeps its files. It reads `http://`,
//! `https://` and `file://`, and refuses everything else.
//!
//! What it cannot do is list a directory, so it reads the index the writer
//! left behind rather than looking to see what is there.

use std::path::PathBuf;

use serde_json::Value;

/// Where a `url` repository points, if it points anywhere allowed.
pub fn url_of(repo: &Value) -> Option<String> {
    if repo.get("type").and_then(|t| t.as_str()) != Some("url") {
        return None;
    }
    repo.pointer("/settings/url")
        .and_then(|v| v.as_str())
        .map(|s| s.trim_end_matches('/').to_string())
}

/// Whether a URL is one this cluster is willing to read.
///
/// A `file://` URL has to sit under the root repositories live in, and every
/// other URL has to be named in `repositories.url.allowed_urls` -- the same
/// two rules OpenSearch applies, and the same message when neither holds.
pub fn allowed(url: &str, allowed_urls: &[String]) -> bool {
    if let Some(path) = url.strip_prefix("file://") {
        let root = super::repo_root();
        let path = PathBuf::from(path);
        return path.starts_with(&root) || root.starts_with(&path);
    }
    allowed_urls.iter().any(|pattern| matches_pattern(pattern, url))
}

/// An allowed URL may end in `*`, which stands for the rest of it.
fn matches_pattern(pattern: &str, url: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => url.starts_with(prefix),
        None => url == pattern || url.starts_with(&format!("{pattern}/")),
    }
}

/// One file out of the repository.
pub fn fetch(url: &str, path: &str) -> Option<Vec<u8>> {
    let full = format!("{url}/{path}");
    if let Some(local) = full.strip_prefix("file://") {
        return std::fs::read(local).ok();
    }
    let response = ureq::get(&full).call().ok()?;
    let mut body = Vec::new();
    use std::io::Read;
    response.into_body().into_reader().read_to_end(&mut body).ok()?;
    Some(body)
}

/// The snapshots a URL repository holds, read from the index its writer left.
pub fn read_records(url: &str) -> Vec<(String, Value)> {
    let Some(index) = fetch(url, "index.json") else { return Vec::new() };
    let Ok(index) = serde_json::from_slice::<Value>(&index) else { return Vec::new() };
    let names = index.get("snapshots").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for name in names.iter().filter_map(|v| v.as_str()) {
        let Some(raw) = fetch(url, &format!("{name}/snapshot.json")) else { continue };
        let Ok(record) = serde_json::from_slice::<Value>(&raw) else { continue };
        out.push((name.to_string(), record));
    }
    out
}
