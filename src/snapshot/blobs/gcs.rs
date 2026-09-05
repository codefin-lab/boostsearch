//! Google Cloud Storage.
//!
//! Two ways in. A service account signs a JWT with its private key and trades
//! it for an access token, which is what a real deployment uses; an emulator,
//! or a bucket reached through a token somebody else obtained, takes the
//! token directly. Everything after that is the JSON API.

use serde_json::Value;

use super::{Store, body_of, failed, under};

pub struct Gcs {
    pub bucket: String,
    pub prefix: String,
    pub endpoint: Option<String>,
    pub credentials: Credentials,
}

pub enum Credentials {
    /// a service account key, as the JSON Google hands out
    ServiceAccount { client_email: String, private_key: String, token_uri: String },
    /// a token somebody else obtained
    Token(String),
    /// an emulator, which asks for nothing
    None,
}

impl Credentials {
    pub fn of(settings: &Value) -> Credentials {
        let text = |key: &str| settings.get(key).and_then(|v| v.as_str()).map(|s| s.to_string());
        if let Some(token) = text("access_token") {
            return Credentials::Token(token);
        }
        // the key may be given inline or as a file, the way gcloud writes it
        let raw = text("credentials_json").or_else(|| {
            let path = text("credentials_file")
                .or_else(|| std::env::var("GOOGLE_APPLICATION_CREDENTIALS").ok())?;
            std::fs::read_to_string(path).ok()
        });
        let Some(parsed) = raw.and_then(|r| serde_json::from_str::<Value>(&r).ok()) else {
            return Credentials::None;
        };
        let field = |key: &str| parsed.get(key).and_then(|v| v.as_str()).map(|s| s.to_string());
        match (field("client_email"), field("private_key")) {
            (Some(client_email), Some(private_key)) => Credentials::ServiceAccount {
                client_email,
                private_key,
                token_uri: field("token_uri")
                    .unwrap_or_else(|| "https://oauth2.googleapis.com/token".into()),
            },
            _ => Credentials::None,
        }
    }
}

impl Gcs {
    fn root(&self) -> String {
        self.endpoint
            .clone()
            .unwrap_or_else(|| "https://storage.googleapis.com".into())
            .trim_end_matches('/')
            .to_string()
    }

    fn key(&self, path: &str) -> String {
        under(&self.prefix, path)
    }

    /// What goes in the Authorization header, if anything does.
    ///
    /// A service account's token is asked for once and kept until it is close
    /// to expiring: a snapshot writes many blobs, and each of them asking
    /// would be a round trip to Google before every one.
    fn token(&self) -> Option<String> {
        use std::sync::{Mutex, OnceLock};
        match &self.credentials {
            Credentials::None => None,
            Credentials::Token(t) => Some(t.clone()),
            Credentials::ServiceAccount { client_email, private_key, token_uri } => {
                static HELD: OnceLock<Mutex<Option<(String, u64)>>> = OnceLock::new();
                let held = HELD.get_or_init(|| Mutex::new(None));
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_secs();
                if let Ok(guard) = held.lock()
                    && let Some((token, until)) = guard.as_ref()
                    && *until > now + 60
                {
                    return Some(token.clone());
                }
                let claims = serde_json::json!({
                    "iss": client_email,
                    "scope": "https://www.googleapis.com/auth/devstorage.read_write",
                    "aud": token_uri,
                    "iat": now,
                    "exp": now + 3600,
                });
                let key = jsonwebtoken::EncodingKey::from_rsa_pem(private_key.as_bytes()).ok()?;
                let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
                let assertion = jsonwebtoken::encode(&header, &claims, &key).ok()?;
                let response = ureq::post(token_uri)
                    .send_form([
                        ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                        ("assertion", assertion.as_str()),
                    ])
                    .ok()?;
                let body = body_of(response)?;
                let parsed: Value = serde_json::from_slice(&body).ok()?;
                let token = parsed.get("access_token")?.as_str()?.to_string();
                let lifetime = parsed.get("expires_in").and_then(|v| v.as_u64()).unwrap_or(3600);
                if let Ok(mut guard) = held.lock() {
                    *guard = Some((token.clone(), now + lifetime));
                }
                Some(token)
            }
        }
    }
}

impl Store for Gcs {
    fn get(&self, path: &str) -> Option<Vec<u8>> {
        let url = format!(
            "{}/storage/v1/b/{}/o/{}?alt=media",
            self.root(),
            self.bucket,
            super::s3::encode_query(&self.key(path))
        );
        let response = with_token(ureq::get(&url), self.token()).call().ok()?;
        (response.status().as_u16() == 200).then(|| body_of(response))?
    }

    fn put(&self, path: &str, bytes: &[u8]) -> std::io::Result<()> {
        let url = format!(
            "{}/upload/storage/v1/b/{}/o?uploadType=media&name={}",
            self.root(),
            self.bucket,
            super::s3::encode_query(&self.key(path))
        );
        let response = with_token(ureq::post(&url), self.token())
            .header("content-type", "application/octet-stream")
            .send(bytes)
            .map_err(|e| failed("gcs put", e))?;
        match response.status().as_u16() {
            200..=299 => Ok(()),
            code => Err(failed("gcs put", format!("status {code}"))),
        }
    }

    fn list(&self, prefix: &str) -> Vec<String> {
        let url = format!(
            "{}/storage/v1/b/{}/o?prefix={}",
            self.root(),
            self.bucket,
            super::s3::encode_query(&self.key(prefix))
        );
        let Ok(response) = with_token(ureq::get(&url), self.token()).call() else {
            return Vec::new();
        };
        let Some(body) = body_of(response) else { return Vec::new() };
        let Ok(parsed) = serde_json::from_slice::<Value>(&body) else { return Vec::new() };
        let under = self.prefix.trim_matches('/');
        parsed
            .get("items")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("name").and_then(|v| v.as_str()))
                    .map(|name| match under.is_empty() {
                        true => name.to_string(),
                        false => name.trim_start_matches(under).trim_start_matches('/').to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn delete(&self, path: &str) -> std::io::Result<()> {
        let url = format!(
            "{}/storage/v1/b/{}/o/{}",
            self.root(),
            self.bucket,
            super::s3::encode_query(&self.key(path))
        );
        let response = with_token(ureq::delete(&url), self.token())
            .call()
            .map_err(|e| failed("gcs delete", e))?;
        match response.status().as_u16() {
            200..=299 | 404 => Ok(()),
            code => Err(failed("gcs delete", format!("status {code}"))),
        }
    }
}

/// A request, carrying the token where there is one to carry.
fn with_token<S>(
    request: ureq::RequestBuilder<S>,
    token: Option<String>,
) -> ureq::RequestBuilder<S> {
    match token {
        Some(token) => request.header("authorization", &format!("Bearer {token}")),
        None => request,
    }
}
