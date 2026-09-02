//! The two processors that hash: a fingerprint of a document's fields, and
//! the Community ID of a network flow.

use serde_json::{Map, Value};

use super::{IngestDoc, IngestError};

/// `fingerprint`: the named fields, sorted, written as `|name|len:value`
/// and hashed; a nested object contributes each of its leaves.
pub fn fingerprint(
    doc: &IngestDoc,
    fields: &[String],
    exclude: &[String],
    method: &str,
    ignore_missing: bool,
) -> Result<Option<String>, IngestError> {
    let metadata = [
        "_index",
        "_id",
        "_routing",
        "_version",
        "_version_type",
        "_if_seq_no",
        "_if_primary_term",
        "_ingest",
    ];
    let mut names: Vec<String> = if !fields.is_empty() {
        let mut v: Vec<String> =
            fields.iter().filter(|f| !metadata.contains(&f.as_str())).cloned().collect();
        v.sort();
        v.dedup();
        v
    } else {
        let mut v: Vec<String> =
            doc.source.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
        v.retain(|f| !metadata.contains(&f.as_str()) && !exclude.contains(f));
        v.sort();
        v
    };
    names.dedup();
    let mut text = String::new();
    for field in &names {
        let Some(value) = doc.get(field) else {
            if ignore_missing {
                continue;
            }
            return Err(IngestError::illegal(format!("field [{field}] doesn't exist")));
        };
        match &value {
            Value::Object(o) => {
                let mut flat: Vec<(String, String)> = Vec::new();
                flatten(o, "", &mut flat);
                flat.sort();
                for (k, v) in flat {
                    text.push_str(&format!("|{field}.{k}|{}:{v}", v.chars().count()));
                }
            }
            other => {
                let v = java_text(other);
                text.push_str(&format!("|{field}|{}:{v}", v.chars().count()));
            }
        }
    }
    if text.is_empty() {
        return Ok(None);
    }
    text.push('|');
    let digest = digest(method, text.as_bytes())?;
    use base64::Engine;
    Ok(Some(format!("{method}:{}", base64::engine::general_purpose::STANDARD.encode(digest))))
}

fn flatten(o: &Map<String, Value>, prefix: &str, out: &mut Vec<(String, String)>) {
    for (k, v) in o {
        let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        match v {
            Value::Object(inner) => flatten(inner, &key, out),
            other => out.push((key, java_text(other))),
        }
    }
}

/// A value the way Java's `String.valueOf` writes it.
pub(crate) fn java_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Array(a) => format!("[{}]", a.iter().map(java_text).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter().map(|(k, v)| format!("{k}={}", java_text(v))).collect::<Vec<_>>().join(", ")
        ),
        Value::Number(n) => {
            if let Some(f) = n.as_f64().filter(|_| !n.is_i64() && !n.is_u64()) {
                if f.fract() == 0.0 && f.abs() < 1e7 { format!("{f:.1}") } else { f.to_string() }
            } else {
                n.to_string()
            }
        }
        Value::Bool(b) => b.to_string(),
    }
}

pub(crate) fn digest(method: &str, bytes: &[u8]) -> Result<Vec<u8>, IngestError> {
    use sha1::Digest as _;
    Ok(match method.to_uppercase().as_str() {
        "MD5@2.16.0" => md5::compute(bytes).0.to_vec(),
        "SHA-1@2.16.0" => sha1::Sha1::digest(bytes).to_vec(),
        "SHA-256@2.16.0" => sha2::Sha256::digest(bytes).to_vec(),
        "SHA3-256@2.16.0" => sha3::Sha3_256::digest(bytes).to_vec(),
        _ => {
            return Err(IngestError::illegal(
                "hash method must be MD5@2.16.0, SHA-1@2.16.0, SHA-256@2.16.0 or SHA3-256@2.16.0",
            ));
        }
    })
}

/// `community_id`: the flow's addresses, ports and protocol, ordered so
/// that both directions hash the same, seeded, and written as `1:` and
/// the base64 of the SHA-1.
pub fn community_id(
    source_ip: &[u8],
    dest_ip: &[u8],
    source_port: u16,
    dest_port: u16,
    protocol: u8,
    seed: u16,
    swap: bool,
) -> String {
    use sha1::Digest as _;
    let (sip, dip, sp, dp) = if swap {
        (dest_ip, source_ip, dest_port, source_port)
    } else {
        (source_ip, dest_ip, source_port, dest_port)
    };
    let mut bytes = Vec::with_capacity(2 + sip.len() + dip.len() + 6);
    bytes.extend_from_slice(&seed.to_be_bytes());
    bytes.extend_from_slice(sip);
    bytes.extend_from_slice(dip);
    bytes.push(protocol);
    bytes.push(0);
    bytes.extend_from_slice(&sp.to_be_bytes());
    bytes.extend_from_slice(&dp.to_be_bytes());
    let digest = sha1::Sha1::digest(&bytes);
    use base64::Engine;
    format!("1:{}", base64::engine::general_purpose::STANDARD.encode(digest))
}

/// The ICMP type a message answers, if it has one; a flow of a request and
/// its reply is one flow.
pub fn icmp_equivalent(v6: bool, ty: u8) -> Option<u8> {
    let table: &[(u8, u8)] = if v6 {
        &[
            (128, 129),
            (129, 128),
            (130, 131),
            (131, 130),
            (133, 134),
            (134, 133),
            (135, 136),
            (136, 135),
            (139, 140),
            (140, 139),
            (144, 145),
            (145, 144),
        ]
    } else {
        &[
            (0, 8),
            (8, 0),
            (9, 10),
            (10, 9),
            (13, 14),
            (14, 13),
            (15, 16),
            (16, 15),
            (17, 18),
            (18, 17),
        ]
    };
    table.iter().find(|(t, _)| *t == ty).map(|(_, c)| *c)
}
