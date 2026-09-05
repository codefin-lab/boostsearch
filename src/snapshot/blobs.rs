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

    fn delete_prefix(&self, prefix: &str) {
        for name in self.list(prefix) {
            let _ = self.delete(&name);
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
