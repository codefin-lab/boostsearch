//! Repositories that are not directories.
//!
//! S3, Google Cloud Storage and Azure Blob Storage all offer the same four
//! things through different doors: read a blob, write a blob, list what is
//! under a prefix, and delete what is under one. What differs is how a
//! request is signed, and that is what each of these modules is.
//!
//! The signing is written here rather than taken from three vendor SDKs. Each
//! of those brings its own async runtime, its own HTTP client and its own
//! error type, for four calls apiece; the signature algorithms are published,
//! stable, and a page of code each.

use serde_json::Value;

pub mod azure;
pub mod gcs;
pub mod s3;

/// What a repository that is not a directory has to be able to do.
pub trait Store: Send + Sync {
    fn get(&self, path: &str) -> Option<Vec<u8>>;
    fn put(&self, path: &str, bytes: &[u8]) -> std::io::Result<()>;
    /// Every blob whose name begins with this, which is how a snapshot is
    /// forgotten: it is a prefix, not a directory.
    fn list(&self, prefix: &str) -> Vec<String>;
    fn delete(&self, path: &str) -> std::io::Result<()>;

    /// Everything a snapshot left behind, and nothing a neighbour did.
    ///
    /// The store is asked for a prefix, but a snapshot is a directory: asking
    /// for `s1` is answered with `s10/docs.ndjson` as well, and deleting one
    /// snapshot took the nine beside it. What is deleted is held to the
    /// directory boundary the caller meant.
    fn delete_prefix(&self, prefix: &str) {
        let within = format!("{}/", prefix.trim_end_matches('/'));
        for name in self.list(prefix) {
            if name == prefix || name.starts_with(&within) {
                let _ = self.delete(&name);
            }
        }
    }
}

/// The store a registered repository stands for, if it stands for one.
pub fn of(repo: &Value) -> Option<Box<dyn Store>> {
    let kind = repo.get("type").and_then(|t| t.as_str())?;
    let settings = repo.get("settings").cloned().unwrap_or(Value::Null);
    let text = |key: &str| settings.get(key).and_then(|v| v.as_str()).map(|s| s.to_string());
    match kind {
        "s3" => Some(Box::new(s3::S3 {
            bucket: text("bucket")?,
            // a repository may live under a prefix of a bucket it shares
            prefix: text("base_path").unwrap_or_default(),
            region: text("region").unwrap_or_else(|| "us-east-1".into()),
            endpoint: text("endpoint"),
            access_key: text("access_key").or_else(|| std::env::var("AWS_ACCESS_KEY_ID").ok())?,
            secret_key: text("secret_key")
                .or_else(|| std::env::var("AWS_SECRET_ACCESS_KEY").ok())?,
            session_token: text("session_token")
                .or_else(|| std::env::var("AWS_SESSION_TOKEN").ok()),
            // an endpoint that is not Amazon's is usually addressed with the
            // bucket in the path rather than in the host
            path_style: settings
                .get("path_style_access")
                .and_then(|v| v.as_bool().or_else(|| v.as_str().map(|s| s == "true")))
                .unwrap_or_else(|| text("endpoint").is_some()),
        })),
        "gcs" => Some(Box::new(gcs::Gcs {
            bucket: text("bucket")?,
            prefix: text("base_path").unwrap_or_default(),
            endpoint: text("endpoint"),
            credentials: gcs::Credentials::of(&settings),
        })),
        "azure" => Some(Box::new(azure::Azure {
            container: text("container")?,
            prefix: text("base_path").unwrap_or_default(),
            account: text("account").or_else(|| std::env::var("AZURE_STORAGE_ACCOUNT").ok())?,
            key: text("key").or_else(|| std::env::var("AZURE_STORAGE_KEY").ok()),
            sas_token: text("sas_token"),
            endpoint: text("endpoint"),
        })),
        _ => None,
    }
}

/// The one HTTP client every repository that is not a directory speaks
/// through.
///
/// It has timeouts, and that is the whole reason it exists. Every call used to
/// be `ureq::get(..)` with none: a repository that stopped answering -- a
/// bucket behind a broken route, a URL repository whose server went away --
/// held the thread that asked it for as long as the operating system was
/// willing to wait. The threads are the runtime's, and `GET /_snapshot/{repo}`
/// reads a read-only repository to see what it holds now, so a handful of
/// those requests took the whole node off the air: the listener was still
/// there and nothing was left to accept.
pub(crate) fn web() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_connect(Some(std::time::Duration::from_secs(10)))
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .build()
            .into()
    })
}

/// A path under the repository's own prefix.
pub(crate) fn under(prefix: &str, path: &str) -> String {
    match prefix.trim_matches('/') {
        "" => path.to_string(),
        p => format!("{p}/{path}"),
    }
}

/// The body of a response, whatever it turned out to be.
pub(crate) fn body_of(response: ureq::http::Response<ureq::Body>) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    response.into_body().into_reader().read_to_end(&mut out).ok()?;
    Some(out)
}

/// What went wrong, as the error kind everything above this expects.
pub(crate) fn failed(what: &str, e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(format!("{what}: {e}"))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// A store that is a list of names, which is all these tests need.
    struct Names(Mutex<Vec<String>>);

    impl Store for Names {
        fn get(&self, _path: &str) -> Option<Vec<u8>> {
            None
        }
        fn put(&self, _path: &str, _bytes: &[u8]) -> std::io::Result<()> {
            Ok(())
        }
        fn list(&self, prefix: &str) -> Vec<String> {
            // what an object store answers: every name beginning with this
            self.0.lock().unwrap().iter().filter(|n| n.starts_with(prefix)).cloned().collect()
        }
        fn delete(&self, path: &str) -> std::io::Result<()> {
            self.0.lock().unwrap().retain(|n| n != path);
            Ok(())
        }
    }

    #[test]
    fn forgetting_one_snapshot_leaves_the_one_named_after_it() {
        let store = Names(Mutex::new(vec![
            "s1/snapshot.json".into(),
            "s1/i/docs.ndjson".into(),
            "s10/snapshot.json".into(),
            "s10/i/docs.ndjson".into(),
            "s1extra".into(),
        ]));
        store.delete_prefix("s1");
        let left = store.0.lock().unwrap().clone();
        assert_eq!(left, vec!["s10/snapshot.json", "s10/i/docs.ndjson", "s1extra"]);
    }
}
