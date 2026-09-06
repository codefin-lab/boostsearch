//! The engine behind the console.
//!
//! The console keeps nothing of its own: a setting somebody changes, a saved
//! object somebody writes, the index patterns the pages read are all in the
//! engine, in an index the console owns. So this is the one place that talks
//! to it, and everything above it asks in terms of documents rather than
//! requests.

use serde_json::{Value, json};

/// Where the console keeps what it knows.
///
/// OpenSearch Dashboards calls it `.kibana`, and the name is part of the
/// contract rather than a choice: a console put in front of a cluster that
/// already has one has to find what is there. It is an alias, and what it
/// points at is [`super::migrate`]'s business.
pub const INDEX: &str = ".kibana";

#[derive(Clone)]
pub struct Engine {
    url: String,
    agent: ureq::Agent,
    auth: Option<String>,
}

/// What went wrong, in a form a handler can answer with.
#[derive(Debug, Clone)]
pub struct Failed {
    pub status: u16,
    pub message: String,
    /// the objects a refusal is about, where it is about particular ones
    pub objects: Option<Vec<Value>>,
}

impl Failed {
    pub fn of(status: u16, message: impl Into<String>) -> Failed {
        Failed { status, message: message.into(), objects: None }
    }

    /// A refusal that names the objects it is about.
    pub fn with_objects(message: impl Into<String>, objects: Vec<Value>) -> Failed {
        Failed { status: 400, message: message.into(), objects: Some(objects) }
    }
}

impl Engine {
    pub fn at(url: &str) -> Engine {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            // a refusal carries a body saying what was wrong with the request,
            // which is the part worth having
            .http_status_as_error(false)
            .build()
            .into();
        // credentials in the URL are how everything else in this repository
        // takes them, so they are taken the same way here
        let (auth, url) = match url.split_once("://") {
            Some((scheme, rest)) => match rest.split_once('@') {
                Some((credentials, host)) if credentials.contains(':') => {
                    use base64::Engine as _;
                    let encoded = base64::engine::general_purpose::STANDARD.encode(credentials);
                    (Some(format!("Basic {encoded}")), format!("{scheme}://{host}"))
                }
                _ => (None, url.to_string()),
            },
            None => (None, url.to_string()),
        };
        Engine { url: url.trim_end_matches('/').to_string(), agent, auth }
    }

    pub fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, Failed> {
        let url = format!("{}{path}", self.url);
        // the two builders are different types -- one may carry a body and one
        // may not -- so the headers go on each rather than on both
        let headed = |request: ureq::RequestBuilder<ureq::typestate::WithBody>| {
            let request = request.header("content-type", "application/json");
            match &self.auth {
                Some(auth) => request.header("authorization", auth),
                None => request,
            }
        };
        let bare = |request: ureq::RequestBuilder<ureq::typestate::WithoutBody>| match &self.auth {
            Some(auth) => request.header("authorization", auth),
            None => request,
        };
        let empty = json!({});
        let sent = match method {
            "GET" => bare(self.agent.get(&url)).call(),
            "DELETE" => bare(self.agent.delete(&url)).call(),
            "PUT" => headed(self.agent.put(&url)).send_json(body.unwrap_or(&empty)),
            "POST" => headed(self.agent.post(&url)).send_json(body.unwrap_or(&empty)),
            other => return Err(Failed::of(500, format!("no such method [{other}]"))),
        };
        let mut answer = sent.map_err(|e| Failed::of(503, format!("{e}")))?;
        let status = answer.status().as_u16();
        // an export of ten thousand objects is tens of megabytes, and the
        // client's own ceiling of ten is a ceiling for a different kind of
        // answer
        let found: Value =
            answer.body_mut().with_config().limit(512 * 1024 * 1024).read_json().map_err(|e| {
                eprintln!("  {method} {path}: the engine's answer could not be read: {e}");
                Failed::of(502, format!("the engine's answer could not be read: {e}"))
            })?;
        match status {
            200..=299 | 404 | 409 => Ok(found),
            other => Err(Failed::of(other, refusal(&found, method, path))),
        }
    }

    /// One saved object, or nothing where there is none.
    pub fn get(&self, id: &str) -> Result<Option<Value>, Failed> {
        let found = self.call("GET", &format!("/{INDEX}/_doc/{}", escape(id)), None)?;
        match found.get("found").and_then(|v| v.as_bool()) {
            Some(true) => Ok(found.get("_source").cloned()),
            _ => Ok(None),
        }
    }

    /// Write one saved object, making the index if it is not there yet.
    ///
    /// `refresh=wait_for` rather than `true`: the front end reads a setting
    /// back the moment it writes one, and a write it cannot read is a setting
    /// that appears not to have been saved.
    pub fn put(&self, id: &str, source: &Value) -> Result<(), Failed> {
        let path = format!("/{INDEX}/_doc/{}?refresh=wait_for&require_alias=true", escape(id));
        let found = self.call("PUT", &path, Some(source))?;
        match found.pointer("/error/type").and_then(|v| v.as_str()) {
            None => Ok(()),
            Some(kind) => Err(Failed::of(500, format!("the console's index: {kind}"))),
        }
    }

    /// Several documents written at once, as the bulk API takes them.
    pub fn bulk(&self, lines: &str) -> Result<Value, Failed> {
        self.bulk_with("refresh=false", lines)
    }

    /// The same, with the caller's own query -- whether to wait for the
    /// refresh, whether the target has to be an alias.
    pub fn bulk_with(&self, query: &str, lines: &str) -> Result<Value, Failed> {
        let url = format!("{}/_bulk?{query}", self.url);
        let mut request = self.agent.post(&url).header("content-type", "application/x-ndjson");
        if let Some(auth) = &self.auth {
            request = request.header("authorization", auth);
        }
        let mut answer =
            request.send(lines.as_bytes()).map_err(|e| Failed::of(503, format!("{e}")))?;
        answer
            .body_mut()
            .read_json()
            .map_err(|e| Failed::of(502, format!("the engine's answer could not be read: {e}")))
    }

    /// Whether the engine is there and will answer.
    pub fn reachable(&self) -> Result<Value, Failed> {
        self.call("GET", "/", None)
    }
}

/// Why the engine refused, in as much detail as it gave.
///
/// "the engine refused the request" is true and useless: the whole reason to
/// pass a message on is that somebody reading a log needs to know which
/// request and what was wrong with it.
fn refusal(found: &Value, method: &str, path: &str) -> String {
    let reason = found
        .pointer("/error/reason")
        .or_else(|| found.pointer("/error/root_cause/0/reason"))
        .or_else(|| found.pointer("/failures/0/cause/reason"))
        .and_then(|v| v.as_str());
    match reason {
        Some(reason) => format!("{method} {path}: {reason}"),
        None => {
            format!("{method} {path}: {}", found.to_string().chars().take(300).collect::<String>())
        }
    }
}

/// An id as a URL path segment.
fn escape(id: &str) -> String {
    super::saved::encoded(id)
}
