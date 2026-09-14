//! On-behalf-of tokens: a short-lived bearer token a caller mints for itself
//! at `POST _plugins/_security/api/generateonbehalfoftoken`, which a service
//! then presents in its place.
//!
//! The token is an HS512 JWT signed with `on_behalf_of.signing_key` (base64),
//! issued by the cluster's name, for the audience the caller named. The
//! caller's security roles travel in the `er` claim, encrypted with
//! `on_behalf_of.encryption_key` the way the plugin encrypts them -- AES with
//! the first sixteen bytes of the decoded key, ECB, PKCS#5 padding -- so a
//! token minted by either reads the same on both.

use std::collections::BTreeMap;

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
use base64::Engine;
use serde_json::{Value, json};

use super::authc::{Credentials, Presented};

/// Tokens live five minutes unless asked otherwise, and never more than ten.
pub const DEFAULT_SECONDS: i64 = 5 * 60;
pub const MAX_SECONDS: i64 = 10 * 60;

/// `config.dynamic.on_behalf_of`
#[derive(Clone, Debug)]
pub struct OboSettings {
    pub enabled: bool,
    signing_key: Vec<u8>,
    encryption_key: [u8; 16],
}

fn text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

impl OboSettings {
    /// The settings, when both keys are there to use; the plugin adds the
    /// authentication domain only then, and mints tokens only when
    /// `enabled` is also true. A signing key shorter than 512 bits is refused
    /// by the plugin as not secure enough, and is not used here either.
    pub fn from_dynamic(dynamic: &Value) -> Option<OboSettings> {
        let o = dynamic.pointer("/dynamic/on_behalf_of")?;
        let signing = text(o.get("signing_key"))?;
        let encryption = text(o.get("encryption_key"))?;
        if signing.len() * 8 < 512 {
            return None;
        }
        let b64 = base64::engine::general_purpose::STANDARD;
        let signing_key = b64.decode(signing.trim()).ok()?;
        let decoded = b64.decode(encryption.trim()).ok()?;
        let mut encryption_key = [0u8; 16];
        let n = decoded.len().min(16);
        encryption_key[..n].copy_from_slice(&decoded[..n]);
        let enabled = match o.get("enabled") {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => s == "true",
            _ => false,
        };
        Some(OboSettings { enabled, signing_key, encryption_key })
    }

    fn encrypt(&self, plain: &str) -> String {
        let cipher = aes::Aes128::new(GenericArray::from_slice(&self.encryption_key));
        let mut data = plain.as_bytes().to_vec();
        let pad = 16 - data.len() % 16;
        data.extend(std::iter::repeat_n(pad as u8, pad));
        for block in data.chunks_mut(16) {
            cipher.encrypt_block(GenericArray::from_mut_slice(block));
        }
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    fn decrypt(&self, encoded: &str) -> Option<String> {
        let mut data = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
        if data.is_empty() || data.len() % 16 != 0 {
            return None;
        }
        let cipher = aes::Aes128::new(GenericArray::from_slice(&self.encryption_key));
        for block in data.chunks_mut(16) {
            cipher.decrypt_block(GenericArray::from_mut_slice(block));
        }
        let pad = *data.last()? as usize;
        if pad == 0 || pad > 16 || data[data.len() - pad..].iter().any(|b| *b as usize != pad) {
            return None;
        }
        data.truncate(data.len() - pad);
        String::from_utf8(data).ok()
    }

    /// A token for `subject`, carrying `roles`, for `audience`, living
    /// `seconds` (at most ten minutes). Answers the token and how long it lives.
    pub fn issue(
        &self,
        subject: &str,
        audience: &str,
        seconds: i64,
        roles: &[String],
    ) -> Result<(String, i64), String> {
        let seconds = seconds.min(MAX_SECONDS);
        if seconds <= 0 {
            return Err("The expiration time should be a positive integer".into());
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let claims = json!({
            "iss": crate::cluster::identity().cluster_name,
            "iat": now,
            "sub": subject,
            "aud": audience,
            "nbf": now,
            "exp": now + seconds,
            "er": self.encrypt(&roles.join(",")),
        });
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS512),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(&self.signing_key),
        )
        .map_err(|e| e.to_string())?;
        Ok((token, seconds))
    }

    /// The caller an on-behalf-of token stands for. A token is not
    /// credentials for minting another token nor for changing a password:
    /// those two requests are read as though no token had been presented.
    pub fn credentials(&self, p: &Presented<'_>) -> Option<Credentials> {
        if !self.enabled {
            return None;
        }
        let header = p.headers.get("authorization")?.to_str().ok()?;
        let lower = header.to_lowercase();
        if !lower.trim_start().starts_with("bearer ") {
            return None;
        }
        let token = header[lower.find("bearer ")? + 7..].trim();
        let suffix = p
            .path
            .strip_prefix("/_plugins/")
            .or_else(|| p.path.strip_prefix("/_opendistro/"))
            .and_then(|rest| rest.split_once('/'))
            .map(|(_, s)| s.trim_end_matches('/'));
        match (suffix, p.method) {
            (Some("api/generateonbehalfoftoken"), "POST") | (Some("api/account"), "PUT") => {
                return None;
            }
            _ => {}
        }
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS512);
        validation.leeway = 0;
        validation.validate_aud = false;
        validation.validate_nbf = true;
        validation.required_spec_claims = Default::default();
        let claims = jsonwebtoken::decode::<Value>(
            token,
            &jsonwebtoken::DecodingKey::from_secret(&self.signing_key),
            &validation,
        )
        .ok()?
        .claims;
        let subject = claims.get("sub").and_then(|v| v.as_str())?.to_string();
        let has_audience = match claims.get("aud") {
            Some(Value::String(s)) => !s.is_empty(),
            Some(Value::Array(a)) => !a.is_empty(),
            _ => false,
        };
        if !has_audience {
            return None;
        }
        if claims.get("iss").and_then(|v| v.as_str())
            != Some(crate::cluster::identity().cluster_name.as_str())
        {
            return None;
        }
        let split = |s: &str| -> Vec<String> {
            s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
        };
        let security_roles = match (claims.get("er"), claims.get("dr")) {
            (Some(er), _) => split(&self.decrypt(er.as_str()?)?),
            (None, Some(dr)) => split(dr.as_str().unwrap_or_default()),
            _ => Vec::new(),
        };
        let backend_roles =
            claims.get("br").and_then(|v| v.as_str()).map(split).unwrap_or_default();
        let mut attributes = BTreeMap::new();
        if let Some(o) = claims.as_object() {
            for (k, v) in o {
                let shown = match v {
                    Value::String(s) => s.clone(),
                    // a list claim is written out as JSON, as the plugin writes it
                    Value::Array(_) => v.to_string(),
                    other => other.to_string(),
                };
                // the audience is a set to the plugin's parser
                let shown =
                    if k == "aud" && !v.is_array() { json!([shown]).to_string() } else { shown };
                attributes.insert(format!("attr.jwt.{k}"), shown);
            }
        }
        Some(Credentials {
            name: subject,
            password: None,
            backend_roles,
            security_roles,
            attributes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> OboSettings {
        let b64 = base64::engine::general_purpose::STANDARD;
        OboSettings::from_dynamic(&json!({"dynamic": {"on_behalf_of": {
            "enabled": true,
            "signing_key": b64.encode([7u8; 64]),
            "encryption_key": b64.encode(b"0123456789abcdef"),
        }}}))
        .unwrap()
    }

    #[test]
    fn roles_encrypt_and_decrypt_as_aes_ecb() {
        let s = settings();
        // AES-128-ECB of "all_access" under "0123456789abcdef", PKCS#5
        // padded: what javax.crypto's "AES" cipher and `openssl enc
        // -aes-128-ecb` both write
        let e = s.encrypt("all_access");
        assert_eq!(e, "/jMSavfQou4F0vGEKIaZ2w==");
        assert_eq!(s.decrypt(&e).as_deref(), Some("all_access"));
        assert!(s.decrypt("not base64!").is_none());
    }

    #[test]
    fn a_short_signing_key_is_not_used() {
        assert!(
            OboSettings::from_dynamic(&json!({"dynamic": {"on_behalf_of": {
                "enabled": true, "signing_key": "c2hvcnQ=", "encryption_key": "c2hvcnQ="
            }}}))
            .is_none()
        );
    }
}
