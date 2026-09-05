//! Azure Blob Storage.
//!
//! Signed with the account's shared key: a canonical form of the request and
//! its headers, signed with HMAC-SHA256, or with a SAS token appended to the
//! URL where somebody has already been given one.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::{Store, body_of, failed, under};

pub struct Azure {
    pub container: String,
    pub prefix: String,
    pub account: String,
    pub key: Option<String>,
    pub sas_token: Option<String>,
    pub endpoint: Option<String>,
}

impl Azure {
    fn root(&self) -> String {
        self.endpoint
            .clone()
            .unwrap_or_else(|| format!("https://{}.blob.core.windows.net", self.account))
            .trim_end_matches('/')
            .to_string()
    }

    fn url(&self, path: &str, query: &str) -> String {
        let blob = under(&self.prefix, path);
        let base = format!("{}/{}/{blob}", self.root(), self.container);
        let sas = self.sas_token.as_deref().unwrap_or("").trim_start_matches('?');
        match (query.is_empty(), sas.is_empty()) {
            (true, true) => base,
            (true, false) => format!("{base}?{sas}"),
            (false, true) => format!("{base}?{query}"),
            (false, false) => format!("{base}?{query}&{sas}"),
        }
    }

    /// What goes in the Authorization header, where a shared key is what
    /// authorises the request rather than a token in the URL.
    ///
    /// The string signed is the method, eleven header fields most of which
    /// are empty, then the `x-ms-` headers in order, then the resource. The
    /// resource is the account followed by the path of the URL -- and against
    /// an emulator, whose URLs carry the account in the path, that means the
    /// account appears twice, which is what the emulator expects because it
    /// is what the rule says.
    fn authorization(
        &self,
        method: &str,
        url: &str,
        query: &[(&str, &str)],
        length: usize,
        headers: &[(&str, String)],
    ) -> Option<String> {
        let key = self.key.as_ref()?;
        let raw = base64_decode(key)?;
        let content_length = if length == 0 { String::new() } else { length.to_string() };
        let content_type = headers
            .iter()
            .find(|(name, _)| *name == "content-type")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        // everything the URL says after the host, which is the resource
        let path = url
            .split_once("://")
            .and_then(|(_, rest)| rest.find('/').map(|at| &rest[at..]))
            .unwrap_or("/");
        let path = path.split('?').next().unwrap_or(path);
        let mut resource = format!("/{}{path}", self.account);
        let mut sorted: Vec<(&str, &str)> = query.to_vec();
        sorted.sort();
        for (name, value) in sorted {
            resource.push_str(&format!("\n{name}:{value}"));
        }
        // only the `x-ms-` headers are canonicalised, in order, and only the
        // ones actually being sent
        let mut ms: Vec<(&str, String)> = headers
            .iter()
            .filter(|(name, _)| name.starts_with("x-ms-"))
            .map(|(name, value)| (*name, value.clone()))
            .collect();
        ms.sort();
        let canonical_headers: String =
            ms.iter().map(|(name, value)| format!("{name}:{value}\n")).collect();
        let to_sign = format!(
            "{method}\n\n\n{content_length}\n\n{content_type}\n\n\n\n\n\n\n{canonical_headers}{resource}"
        );
        let mut mac = <Hmac<Sha256>>::new_from_slice(&raw).ok()?;
        mac.update(to_sign.as_bytes());
        use base64::Engine;
        let signature =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        Some(format!("SharedKey {}:{signature}", self.account))
    }

    /// The headers every request carries, and the ones this one adds.
    fn common(&self, extra: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
        let mut out: Vec<(&'static str, String)> =
            vec![("x-ms-date", Self::now()), ("x-ms-version", "2021-08-06".to_string())];
        out.extend(extra.iter().cloned());
        out
    }

    /// The date every signed request carries, in the one format accepted.
    fn now() -> String {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        crate::store::format_millis(seconds as i64 * 1000, "EEE, dd MMM yyyy HH:mm:ss 'GMT'")
            .unwrap_or_default()
    }
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(text.trim()).ok()
}

impl Store for Azure {
    fn get(&self, path: &str) -> Option<Vec<u8>> {
        let url = self.url(path, "");
        let headers = self.common(&[]);
        let mut request = ureq::get(&url);
        for (name, value) in &headers {
            request = request.header(*name, value);
        }
        if let Some(auth) = self.authorization("GET", &url, &[], 0, &headers) {
            request = request.header("authorization", &auth);
        }
        let response = request.call().ok()?;
        (response.status().as_u16() == 200).then(|| body_of(response))?
    }

    fn put(&self, path: &str, bytes: &[u8]) -> std::io::Result<()> {
        let url = self.url(path, "");
        let headers = self.common(&[
            ("x-ms-blob-type", "BlockBlob".to_string()),
            ("content-type", "application/octet-stream".to_string()),
        ]);
        let mut request = ureq::put(&url);
        for (name, value) in &headers {
            request = request.header(*name, value);
        }
        if let Some(auth) = self.authorization("PUT", &url, &[], bytes.len(), &headers) {
            request = request.header("authorization", &auth);
        }
        let response = request.send(bytes).map_err(|e| failed("azure put", e))?;
        match response.status().as_u16() {
            200..=299 => Ok(()),
            code => Err(failed("azure put", format!("status {code}"))),
        }
    }

    fn list(&self, prefix: &str) -> Vec<String> {
        let full = under(&self.prefix, prefix);
        let url = format!(
            "{}/{}?restype=container&comp=list&prefix={}",
            self.root(),
            self.container,
            super::s3::encode_query(&full)
        );
        // a listing is of the container, so the query is part of what is
        // signed rather than something hung off the end of a blob's name
        let query = [("comp", "list"), ("prefix", full.as_str()), ("restype", "container")];
        let headers = self.common(&[]);
        let signed = self.authorization("GET", &url, &query, 0, &headers);
        let url = match self.sas_token.as_deref() {
            Some(sas) => format!("{url}&{}", sas.trim_start_matches('?')),
            None => url,
        };
        let mut request = ureq::get(&url);
        for (name, value) in &headers {
            request = request.header(*name, value);
        }
        if let Some(auth) = signed {
            request = request.header("authorization", &auth);
        }
        let Ok(response) = request.call() else { return Vec::new() };
        let Some(body) = body_of(response) else { return Vec::new() };
        let text = String::from_utf8_lossy(&body);
        let under_prefix = self.prefix.trim_matches('/');
        text.split("<Name>")
            .skip(1)
            .filter_map(|part| part.split("</Name>").next())
            .map(|name| match under_prefix.is_empty() {
                true => name.to_string(),
                false => name.trim_start_matches(under_prefix).trim_start_matches('/').to_string(),
            })
            .collect()
    }

    fn delete(&self, path: &str) -> std::io::Result<()> {
        let url = self.url(path, "");
        let headers = self.common(&[]);
        let mut request = ureq::delete(&url);
        for (name, value) in &headers {
            request = request.header(*name, value);
        }
        if let Some(auth) = self.authorization("DELETE", &url, &[], 0, &headers) {
            request = request.header("authorization", &auth);
        }
        let response = request.call().map_err(|e| failed("azure delete", e))?;
        match response.status().as_u16() {
            200..=299 | 404 => Ok(()),
            code => Err(failed("azure delete", format!("status {code}"))),
        }
    }
}
