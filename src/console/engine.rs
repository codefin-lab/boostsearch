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
/// already has one has to find what is there.
pub const INDEX: &str = ".kibana";

#[derive(Clone)]
pub struct Engine {
    url: String,
    agent: ureq::Agent,
    auth: Option<String>,
}

/// What went wrong, in a form a handler can answer with.
#[derive(Debug)]
pub struct Failed {
    pub status: u16,
    pub message: String,
}

impl Failed {
    fn of(status: u16, message: impl Into<String>) -> Failed {
        Failed { status, message: message.into() }
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

    fn send(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, Failed> {
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
        let found: Value = answer
            .body_mut()
            .read_json()
            .map_err(|e| Failed::of(502, format!("the engine's answer could not be read: {e}")))?;
        match status {
            200..=299 | 404 | 409 => Ok(found),
            other => Err(Failed::of(
                other,
                found
                    .pointer("/error/reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("the engine refused the request")
                    .to_string(),
            )),
        }
    }

    /// One saved object, or nothing where there is none.
    pub fn get(&self, id: &str) -> Result<Option<Value>, Failed> {
        let found = self.send("GET", &format!("/{INDEX}/_doc/{}", escape(id)), None)?;
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
        let path = format!("/{INDEX}/_doc/{}?refresh=wait_for", escape(id));
        self.send("PUT", &path, Some(source))?;
        Ok(())
    }

    /// Whether the engine is there and will answer.
    pub fn reachable(&self) -> Result<Value, Failed> {
        self.send("GET", "/", None)
    }

    /// Make the console's index if nothing has yet.
    pub fn ensure_index(&self) -> Result<(), Failed> {
        let found = self.send("GET", &format!("/{INDEX}"), None)?;
        if found.get("error").is_none() {
            return Ok(());
        }
        // a mapping the saved objects need: the type decides which of the
        // per-type property bags a document's fields live in
        let made = self.send(
            "PUT",
            &format!("/{INDEX}"),
            Some(&json!({
                "settings": {"number_of_shards": 1},
                "mappings": {"dynamic": true, "properties": {
                    "type": {"type": "keyword"},
                    "updated_at": {"type": "date"},
                    "references": {"type": "nested", "properties": {
                        "name": {"type": "keyword"},
                        "type": {"type": "keyword"},
                        "id": {"type": "keyword"},
                    }},
                }},
            })),
        )?;
        // somebody else making it at the same moment is not a failure
        match made.pointer("/error/type").and_then(|v| v.as_str()) {
            Some("resource_already_exists_exception") | None => Ok(()),
            Some(other) => Err(Failed::of(500, format!("the console's index: {other}"))),
        }
    }
}

/// An id as a URL path segment.
fn escape(id: &str) -> String {
    id.replace('%', "%25").replace('/', "%2F").replace('#', "%23").replace('?', "%3F")
}
