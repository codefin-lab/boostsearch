//! Short URLs.
//!
//! A dashboard's address carries its whole state -- the time range, the
//! filters, the query -- and is too long to send to anybody. The share
//! menu asks for a short one: the console keeps the long address as a saved
//! object of type `url` under the MD5 of the address, and `/goto/{id}`
//! sends the browser to it. The same address always gets the same id,
//! which is why a conflict on the write is the answer rather than a
//! failure.

use serde_json::json;

use super::engine::Failed;
use super::saved::{Saved, Writing};

/// What a short URL may point at: a path under `/app/`, with no scheme
/// and no host, so that a link somebody shares cannot send a reader off
/// this console.
pub fn assert_valid(url: &str) -> Result<(), Failed> {
    let refused = |m: String| Failed::of(406, m);
    if let Some((scheme, _)) = url.split_once("://")
        && !scheme.is_empty()
        && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    {
        return Err(refused(format!(
            "Short url targets cannot have a protocol, found \"{scheme}:\""
        )));
    }
    if let Some(rest) = url.strip_prefix("//") {
        let host: String = rest.chars().take_while(|c| !"/?#".contains(*c)).collect();
        if !host.is_empty() {
            return Err(refused(format!(
                "Short url targets cannot have a hostname, found \"{host}\""
            )));
        }
    }
    let path: &str = url.split(['?', '#']).next().unwrap_or("");
    let mut parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    // a workspace prefix is skipped over, as the server being replaced does
    if parts.first() == Some(&"w") {
        parts = parts.into_iter().skip(2).collect();
    }
    if parts.first() != Some(&"app") || parts.get(1).is_none_or(|p| p.is_empty()) {
        return Err(refused(format!(
            "Short url target path must be in the format \"/app/{{appId}}\", found \"{path}\""
        )));
    }
    Ok(())
}

/// The id of a long address, made or already there.
pub fn shorten(saved: &Saved<'_>, url: &str) -> Result<String, Failed> {
    assert_valid(url)?;
    let id = format!("{:x}", md5::compute(url.as_bytes()));
    let now = now_millis();
    let writing = Writing {
        kind: "url".to_string(),
        id: Some(id.clone()),
        attributes: json!({"url": url, "accessCount": 0, "createDate": now, "accessDate": now}),
        references: json!([]),
        migration_version: None,
        overwrite: false,
    };
    match saved.create(writing) {
        Ok(_) => Ok(id),
        Err(e) if e.status == 409 => Ok(id),
        Err(e) => Err(e),
    }
}

/// The long address behind an id, and a note that somebody followed it.
pub fn resolve(saved: &Saved<'_>, id: &str) -> Result<String, Failed> {
    let found = saved.get("url", id)?;
    let url = found
        .pointer("/attributes/url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Failed::of(404, format!("Saved object [url/{id}] not found")))?
        .to_string();
    assert_valid(&url)?;
    let count = found.pointer("/attributes/accessCount").and_then(|v| v.as_u64()).unwrap_or(0);
    // the count is bookkeeping; a reader who cannot be counted is still sent on
    let _ = saved.update(
        "url",
        id,
        &json!({"accessDate": now_millis(), "accessCount": count + 1}),
        None,
    );
    Ok(url)
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_path_under_app_is_a_target() {
        assert!(assert_valid("/app/visualize#/create").is_ok());
        assert!(assert_valid("/app/dashboards?x=1").is_ok());
        assert!(assert_valid("/w/ws1/app/dashboards").is_ok());
        let refused = |url: &str| assert_valid(url).unwrap_err();
        assert_eq!(refused("http://elsewhere/app/x").status, 406);
        assert!(refused("http://elsewhere/app/x").message.contains("protocol"));
        assert!(refused("//elsewhere/app/x").message.contains("hostname"));
        assert!(refused("/api/status").message.contains("/app/{appId}"));
        assert!(refused("/app/").message.contains("/app/{appId}"));
    }
}
