//! S3, and anything that speaks its protocol.
//!
//! Requests are signed with AWS Signature Version 4, which is a published
//! algorithm: hash the request into a canonical form, sign that with a key
//! derived from the date, the region and the service, and send both the
//! signature and a list of which headers went into it.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use super::{Store, body_of, failed, under};

pub struct S3 {
    pub bucket: String,
    pub prefix: String,
    pub region: String,
    pub endpoint: Option<String>,
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
    pub path_style: bool,
}

impl S3 {
    /// Where a blob is, as a URL.
    fn url(&self, key: &str) -> String {
        match (&self.endpoint, self.path_style) {
            (Some(endpoint), true) => {
                format!("{}/{}/{key}", endpoint.trim_end_matches('/'), self.bucket)
            }
            (Some(endpoint), false) => {
                let endpoint = endpoint.trim_end_matches('/');
                let (scheme, host) = endpoint.split_once("://").unwrap_or(("https", endpoint));
                format!("{scheme}://{}.{host}/{key}", self.bucket)
            }
            (None, true) => {
                format!("https://s3.{}.amazonaws.com/{}/{key}", self.region, self.bucket)
            }
            (None, false) => {
                format!("https://{}.s3.{}.amazonaws.com/{key}", self.bucket, self.region)
            }
        }
    }

    /// The host a signature is computed against, which is the one the request
    /// is actually sent to.
    fn host_of(url: &str) -> String {
        url.split_once("://")
            .map(|(_, rest)| rest.split('/').next().unwrap_or_default().to_string())
            .unwrap_or_default()
    }

    /// Sign a request, and send it.
    fn send(
        &self,
        method: &str,
        url: &str,
        query: &str,
        body: &[u8],
    ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let stamp = timestamp(now);
        let day = &stamp[..8];
        let payload_hash = hex(&Sha256::digest(body));
        let host = Self::host_of(url);
        let path = url.split_once("://").map_or("/", |(_, rest)| match rest.find('/') {
            Some(at) => &rest[at..],
            None => "/",
        });
        // The canonical request is the shape both ends agree to hash. Only
        // three headers are signed: any more and every proxy between here and
        // there becomes part of the signature.
        let canonical_headers = match &self.session_token {
            Some(token) => format!(
                "host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{stamp}\nx-amz-security-token:{token}\n"
            ),
            None => {
                format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{stamp}\n")
            }
        };
        let signed_headers = match self.session_token {
            Some(_) => "host;x-amz-content-sha256;x-amz-date;x-amz-security-token",
            None => "host;x-amz-content-sha256;x-amz-date",
        };
        let canonical = format!(
            "{method}\n{}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
            encode_path(path)
        );
        let scope = format!("{day}/{}/s3/aws4_request", self.region);
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
            hex(&Sha256::digest(canonical.as_bytes()))
        );
        let signature = hex(&self.signing_key(day).chain(to_sign.as_bytes()));
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, \
             Signature={signature}",
            self.access_key
        );
        let full = if query.is_empty() { url.to_string() } else { format!("{url}?{query}") };
        // the headers that were signed have to be the headers that are sent,
        // and ureq keeps a request with a body and one without in different
        // types, so the two are built separately from the same list
        let mut headers: Vec<(&str, String)> = vec![
            ("host", host.clone()),
            ("x-amz-date", stamp.clone()),
            ("x-amz-content-sha256", payload_hash.clone()),
            ("authorization", authorization),
        ];
        if let Some(token) = &self.session_token {
            headers.push(("x-amz-security-token", token.clone()));
        }
        if method == "PUT" {
            let mut request = ureq::put(&full);
            for (name, value) in &headers {
                request = request.header(*name, value);
            }
            return request.send(body);
        }
        let mut request = match method {
            "DELETE" => ureq::delete(&full),
            _ => ureq::get(&full),
        };
        for (name, value) in &headers {
            request = request.header(*name, value);
        }
        request.call()
    }

    /// The key derived from the secret for one day, one region and one
    /// service, which is what actually signs a request.
    fn signing_key(&self, day: &str) -> Chain {
        let key = format!("AWS4{}", self.secret_key);
        let date = sign(key.as_bytes(), day.as_bytes());
        let region = sign(&date, self.region.as_bytes());
        let service = sign(&region, b"s3");
        Chain(sign(&service, b"aws4_request"))
    }

    fn key(&self, path: &str) -> String {
        under(&self.prefix, path)
    }
}

/// A key waiting for the thing it will sign.
pub(crate) struct Chain(Vec<u8>);

impl Chain {
    fn chain(&self, message: &[u8]) -> Vec<u8> {
        sign(&self.0, message)
    }
}

impl Store for S3 {
    fn get(&self, path: &str) -> Option<Vec<u8>> {
        let key = self.key(path);
        let response = self.send("GET", &self.url(&key), "", &[]).ok()?;
        (response.status().as_u16() == 200).then(|| body_of(response))?
    }

    fn put(&self, path: &str, bytes: &[u8]) -> std::io::Result<()> {
        let key = self.key(path);
        let response =
            self.send("PUT", &self.url(&key), "", bytes).map_err(|e| failed("s3 put", e))?;
        match response.status().as_u16() {
            200..=299 => Ok(()),
            code => Err(failed("s3 put", format!("status {code}"))),
        }
    }

    fn list(&self, prefix: &str) -> Vec<String> {
        let full = self.key(prefix);
        // the bucket is listed rather than the object, so the query goes to
        // the bucket's own URL and the prefix goes in the query string
        let root = self.url("");
        let root = root.trim_end_matches('/').to_string();
        let query = format!("list-type=2&prefix={}", encode_query(&full));
        let Ok(response) = self.send("GET", &root, &query, &[]) else { return Vec::new() };
        let Some(body) = body_of(response) else { return Vec::new() };
        let text = String::from_utf8_lossy(&body);
        // the answer is XML, and what is wanted from it is every <Key>
        let mut out = Vec::new();
        for part in text.split("<Key>").skip(1) {
            let Some(key) = part.split("</Key>").next() else { continue };
            let under = self.prefix.trim_matches('/');
            let name = match under.is_empty() {
                true => key.to_string(),
                false => key.trim_start_matches(under).trim_start_matches('/').to_string(),
            };
            out.push(name);
        }
        out
    }

    fn delete(&self, path: &str) -> std::io::Result<()> {
        let key = self.key(path);
        let response =
            self.send("DELETE", &self.url(&key), "", &[]).map_err(|e| failed("s3 delete", e))?;
        match response.status().as_u16() {
            200..=299 | 404 => Ok(()),
            code => Err(failed("s3 delete", format!("status {code}"))),
        }
    }
}

fn sign(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256>>::new_from_slice(key).expect("hmac takes a key of any length");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `20240101T000000Z`, which is the only date format a signature accepts.
pub(crate) fn timestamp(seconds: u64) -> String {
    crate::store::format_millis(seconds as i64 * 1000, "yyyyMMdd'T'HHmmss'Z'").unwrap_or_default()
}

/// A path as a signature wants it: every segment escaped, the slashes left.
fn encode_path(path: &str) -> String {
    path.split('/').map(encode_query).collect::<Vec<_>>().join("/")
}

/// One value, escaped the way a signature and a query string both want.
pub(crate) fn encode_query(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}
