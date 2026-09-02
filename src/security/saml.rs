//! SAML 2.0 as the plugin does it: Dashboards sends the browser to the
//! IdP with an `AuthnRequest`, the IdP posts a signed `Response` back to
//! Dashboards, and Dashboards hands it to `_plugins/_security/api/authtoken`,
//! which checks it and answers with a JWT signed by the exchange key. That
//! JWT is then what every later request carries, and the `saml` domain
//! reads it as a `jwt` domain would.
//!
//! The XML signature is checked here in full: the `SignedInfo` is
//! canonicalised (exclusive C14N), the referenced element is canonicalised
//! with the signature taken out, digests are compared and the signature is
//! verified against the certificates the IdP's metadata names.

use std::collections::BTreeMap;

use serde_json::{Value, json};

// ---- a small DOM ------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Node {
    pub prefix: Option<String>,
    pub local: String,
    /// namespace declarations on this element: prefix ("" for default) -> uri
    pub ns_decls: Vec<(String, String)>,
    /// attributes as written: (prefix, local, value)
    pub attrs: Vec<(Option<String>, String, String)>,
    pub children: Vec<Child>,
}

#[derive(Clone, Debug)]
pub enum Child {
    Element(Node),
    Text(String),
}

fn split_qname(q: &str) -> (Option<String>, String) {
    match q.split_once(':') {
        Some((p, l)) => (Some(p.to_string()), l.to_string()),
        None => (None, q.to_string()),
    }
}

/// Parse a document into the tree; entities and character references are
/// resolved, comments and processing instructions dropped.
pub fn parse(xml: &str) -> Option<Node> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;
    loop {
        match reader.read_event().ok()? {
            Event::Start(e) => {
                let name = e.name().as_ref().to_string();
                let (prefix, local) = split_qname(&name);
                let mut node = Node {
                    prefix,
                    local,
                    ns_decls: Vec::new(),
                    attrs: Vec::new(),
                    children: Vec::new(),
                };
                for a in e.attributes().flatten() {
                    let key = a.key.as_ref().to_string();
                    let value =
                        a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok()?.to_string();
                    if key == "xmlns" {
                        node.ns_decls.push((String::new(), value));
                    } else if let Some(p) = key.strip_prefix("xmlns:") {
                        node.ns_decls.push((p.to_string(), value));
                    } else {
                        let (p, l) = split_qname(&key);
                        node.attrs.push((p, l, value));
                    }
                }
                stack.push(node);
            }
            Event::End(_) => {
                let node = stack.pop()?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(Child::Element(node)),
                    None => root = Some(node),
                }
            }
            Event::Text(t) => {
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(Child::Text(
                        t.xml_content(quick_xml::XmlVersion::Implicit1_0).to_string(),
                    ));
                }
            }
            Event::CData(c) => {
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(Child::Text(c.as_ref().to_string()));
                }
            }
            // `&amp;`, `&#10;` and the like arrive as references of their own
            Event::GeneralRef(r) => {
                let name = r.as_ref().to_string();
                let resolved = match name.as_str() {
                    "amp" => Some("&".to_string()),
                    "lt" => Some("<".to_string()),
                    "gt" => Some(">".to_string()),
                    "quot" => Some("\"".to_string()),
                    "apos" => Some("'".to_string()),
                    n if n.starts_with("#x") => u32::from_str_radix(&n[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                        .map(|c| c.to_string()),
                    n if n.starts_with('#') => {
                        n[1..].parse::<u32>().ok().and_then(char::from_u32).map(|c| c.to_string())
                    }
                    _ => None,
                };
                let text = resolved.unwrap_or_else(|| format!("&{name};"));
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(Child::Text(text));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    root
}

impl Node {
    pub fn attr(&self, local: &str) -> Option<&str> {
        self.attrs.iter().find(|(_, l, _)| l == local).map(|(_, _, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        let mut s = String::new();
        for c in &self.children {
            match c {
                Child::Text(t) => s.push_str(t),
                Child::Element(e) => s.push_str(&e.text()),
            }
        }
        s
    }

    pub fn child(&self, local: &str) -> Option<&Node> {
        self.children.iter().find_map(|c| match c {
            Child::Element(e) if e.local == local => Some(e),
            _ => None,
        })
    }

    pub fn children_named<'a>(&'a self, local: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter_map(move |c| match c {
            Child::Element(e) if e.local == local => Some(e),
            _ => None,
        })
    }

    /// Every descendant (and self) with this local name, document order.
    pub fn find_all<'a>(&'a self, local: &str, out: &mut Vec<&'a Node>) {
        if self.local == local {
            out.push(self);
        }
        for c in &self.children {
            if let Child::Element(e) = c {
                e.find_all(local, out);
            }
        }
    }

    pub fn find_first(&self, local: &str) -> Option<&Node> {
        let mut v = Vec::new();
        self.find_all(local, &mut v);
        v.into_iter().next()
    }

    /// The element with this `ID`, anywhere below.
    pub fn by_id(&self, id: &str) -> Option<&Node> {
        if self.attr("ID") == Some(id) {
            return Some(self);
        }
        for c in &self.children {
            if let Child::Element(e) = c {
                if let Some(f) = e.by_id(id) {
                    return Some(f);
                }
            }
        }
        None
    }

    /// The same tree without any `Signature` element.
    fn without_signature(&self) -> Node {
        let mut n = self.clone();
        n.children = self
            .children
            .iter()
            .filter(|c| !matches!(c, Child::Element(e) if e.local == "Signature"))
            .map(|c| match c {
                Child::Element(e) => Child::Element(e.without_signature()),
                Child::Text(t) => Child::Text(t.clone()),
            })
            .collect();
        n
    }
}

// ---- exclusive canonicalisation --------------------------------------------------

fn c14n_escape_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\r', "&#xD;")
}

fn c14n_escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('"', "&quot;")
        .replace('\t', "&#x9;")
        .replace('\n', "&#xA;")
        .replace('\r', "&#xD;")
}

/// Exclusive XML canonicalisation of one element and what is under it,
/// `inclusive` naming the prefixes to carry as inclusive C14N would.
/// `scope` holds the namespaces in force from ancestors of the element in
/// the source document (needed to resolve prefixes), `rendered` the ones
/// an output ancestor already wrote.
pub fn c14n_exclusive(
    node: &Node,
    scope: &BTreeMap<String, String>,
    inclusive: &[String],
) -> String {
    let mut out = String::new();
    let rendered: BTreeMap<String, String> = BTreeMap::new();
    write_c14n(node, scope, &rendered, inclusive, &mut out);
    out
}

fn write_c14n(
    node: &Node,
    scope: &BTreeMap<String, String>,
    rendered: &BTreeMap<String, String>,
    inclusive: &[String],
    out: &mut String,
) {
    // namespaces in force here
    let mut here = scope.clone();
    for (p, u) in &node.ns_decls {
        here.insert(p.clone(), u.clone());
    }
    // the prefixes this element visibly uses: its own, and its attributes'
    let mut used: Vec<String> = vec![node.prefix.clone().unwrap_or_default()];
    for (p, _, _) in &node.attrs {
        if let Some(p) = p {
            if p != "xml" && !used.contains(p) {
                used.push(p.clone());
            }
        }
    }
    for p in inclusive {
        if !used.contains(p) {
            used.push(p.clone());
        }
    }
    let mut ns_out: Vec<(String, String)> = Vec::new();
    for p in &used {
        let uri = match here.get(p) {
            Some(u) => u.clone(),
            None => {
                if p.is_empty() {
                    continue;
                }
                continue;
            }
        };
        let already = rendered.get(p).map(|u| u == &uri).unwrap_or(false);
        if !already && !(p.is_empty() && uri.is_empty() && rendered.get("").is_none()) {
            ns_out.push((p.clone(), uri));
        }
    }
    ns_out.sort();
    let mut next_rendered = rendered.clone();
    for (p, u) in &ns_out {
        next_rendered.insert(p.clone(), u.clone());
    }
    // attributes sorted by namespace uri then local name
    let mut attrs: Vec<(String, String, String, Option<String>)> = node
        .attrs
        .iter()
        .map(|(p, l, v)| {
            let uri = match p {
                Some(pp) if pp == "xml" => "http://www.w3.org/XML/1998/namespace".to_string(),
                Some(pp) => here.get(pp).cloned().unwrap_or_default(),
                None => String::new(),
            };
            (uri, l.clone(), v.clone(), p.clone())
        })
        .collect();
    attrs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let qname = match &node.prefix {
        Some(p) => format!("{p}:{}", node.local),
        None => node.local.clone(),
    };
    out.push('<');
    out.push_str(&qname);
    for (p, u) in &ns_out {
        if p.is_empty() {
            out.push_str(&format!(" xmlns=\"{}\"", c14n_escape_attr(u)));
        } else {
            out.push_str(&format!(" xmlns:{p}=\"{}\"", c14n_escape_attr(u)));
        }
    }
    for (_, l, v, p) in &attrs {
        match p {
            Some(pp) => out.push_str(&format!(" {pp}:{l}=\"{}\"", c14n_escape_attr(v))),
            None => out.push_str(&format!(" {l}=\"{}\"", c14n_escape_attr(v))),
        }
    }
    out.push('>');
    for c in &node.children {
        match c {
            Child::Text(t) => out.push_str(&c14n_escape_text(t)),
            Child::Element(e) => write_c14n(e, &here, &next_rendered, inclusive, out),
        }
    }
    out.push_str("</");
    out.push_str(&qname);
    out.push('>');
}

/// The namespaces in force at a node found under `root`, by walking down
/// to it: what the source document's ancestors declared.
fn scope_at<'a>(
    root: &'a Node,
    target: &Node,
    scope: &mut BTreeMap<String, String>,
) -> Option<BTreeMap<String, String>> {
    let mut here = scope.clone();
    for (p, u) in &root.ns_decls {
        here.insert(p.clone(), u.clone());
    }
    if std::ptr::eq(root, target) {
        // the scope *outside* the target: without its own declarations
        return Some(scope.clone());
    }
    for c in &root.children {
        if let Child::Element(e) = c {
            if let Some(found) = scope_at(e, target, &mut here) {
                return Some(found);
            }
        }
    }
    None
}

// ---- signature verification ------------------------------------------------------

/// The certificates (DER) an IdP signs with, from its metadata.
pub fn signing_certs(metadata: &Node) -> Vec<Vec<u8>> {
    use base64::Engine;
    let mut out = Vec::new();
    let mut kds = Vec::new();
    metadata.find_all("KeyDescriptor", &mut kds);
    for kd in kds {
        let use_ = kd.attr("use").unwrap_or("");
        if use_ != "signing" && !use_.is_empty() {
            continue;
        }
        let mut certs = Vec::new();
        kd.find_all("X509Certificate", &mut certs);
        for c in certs {
            let text: String = c.text().chars().filter(|ch| !ch.is_whitespace()).collect();
            if let Ok(der) = base64::engine::general_purpose::STANDARD.decode(&text) {
                out.push(der);
            }
        }
    }
    out
}

fn rsa_public_key(cert_der: &[u8]) -> Option<rsa::RsaPublicKey> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der).ok()?;
    let spki = cert.public_key();
    use rsa::pkcs1::DecodeRsaPublicKey;
    rsa::RsaPublicKey::from_pkcs1_der(spki.subject_public_key.data.as_ref()).ok()
}

/// Whether one `Signature` element under `root` is valid for the element
/// it references, by one of the certificates.
fn verify_signature(root: &Node, sig: &Node, certs: &[Vec<u8>]) -> Option<String> {
    let signed_info = sig.child("SignedInfo")?;
    let c14n_method =
        signed_info.child("CanonicalizationMethod").and_then(|m| m.attr("Algorithm")).unwrap_or("");
    let sig_method =
        signed_info.child("SignatureMethod").and_then(|m| m.attr("Algorithm")).unwrap_or("");
    let reference = signed_info.child("Reference")?;
    let uri = reference.attr("URI").unwrap_or("");
    let digest_method =
        reference.child("DigestMethod").and_then(|m| m.attr("Algorithm")).unwrap_or("");
    let digest_value: String =
        reference.child("DigestValue")?.text().chars().filter(|c| !c.is_whitespace()).collect();
    let signature_value: String =
        sig.child("SignatureValue")?.text().chars().filter(|c| !c.is_whitespace()).collect();
    let inclusive: Vec<String> = signed_info
        .child("CanonicalizationMethod")
        .and_then(|m| m.find_first("InclusiveNamespaces"))
        .and_then(|i| i.attr("PrefixList"))
        .map(|l| l.split_whitespace().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    let ref_inclusive: Vec<String> = reference
        .find_first("InclusiveNamespaces")
        .and_then(|i| i.attr("PrefixList"))
        .map(|l| l.split_whitespace().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    if !c14n_method.starts_with("http://www.w3.org/2001/10/xml-exc-c14n#") {
        return Some(format!("unsupported canonicalisation {c14n_method}"));
    }
    // the referenced element: `#id`, or the whole document when empty
    let target: &Node =
        if uri.is_empty() { root } else { root.by_id(uri.trim_start_matches('#'))? };
    let mut empty = BTreeMap::new();
    let scope = scope_at(root, target, &mut empty).unwrap_or_default();
    let stripped = target.without_signature();
    let canon = c14n_exclusive(&stripped, &scope, &ref_inclusive);
    use base64::Engine;
    let want = base64::engine::general_purpose::STANDARD.decode(&digest_value).ok()?;
    let have: Vec<u8> = match digest_method {
        "http://www.w3.org/2001/04/xmlenc#sha256" => {
            use sha2::Digest as _;
            sha2::Sha256::digest(canon.as_bytes()).to_vec()
        }
        "http://www.w3.org/2000/09/xmldsig#sha1" => {
            use sha1::Digest as _;
            sha1::Sha1::digest(canon.as_bytes()).to_vec()
        }
        "http://www.w3.org/2001/04/xmlenc#sha512" => {
            use sha2::Digest as _;
            sha2::Sha512::digest(canon.as_bytes()).to_vec()
        }
        other => return Some(format!("unsupported digest {other}")),
    };
    if want != have {
        return Some("digest mismatch".into());
    }
    // SignedInfo canonicalised in its own scope
    let si_scope = scope_at(root, signed_info, &mut BTreeMap::new()).unwrap_or_default();
    let si_canon = c14n_exclusive(signed_info, &si_scope, &inclusive);
    let sig_bytes = base64::engine::general_purpose::STANDARD.decode(&signature_value).ok()?;
    for cert in certs {
        let Some(pk) = rsa_public_key(cert) else { continue };
        let ok = match sig_method {
            "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256" => {
                use rsa::signature::Verifier as _;
                let vk = rsa::pkcs1v15::VerifyingKey::<sha2::Sha256>::new(pk);
                rsa::pkcs1v15::Signature::try_from(sig_bytes.as_slice())
                    .map(|s| vk.verify(si_canon.as_bytes(), &s).is_ok())
                    .unwrap_or(false)
            }
            "http://www.w3.org/2000/09/xmldsig#rsa-sha1" => {
                use rsa::signature::Verifier as _;
                let vk = rsa::pkcs1v15::VerifyingKey::<sha1::Sha1>::new(pk);
                rsa::pkcs1v15::Signature::try_from(sig_bytes.as_slice())
                    .map(|s| vk.verify(si_canon.as_bytes(), &s).is_ok())
                    .unwrap_or(false)
            }
            "http://www.w3.org/2001/04/xmldsig-more#rsa-sha512" => {
                use rsa::signature::Verifier as _;
                let vk = rsa::pkcs1v15::VerifyingKey::<sha2::Sha512>::new(pk);
                rsa::pkcs1v15::Signature::try_from(sig_bytes.as_slice())
                    .map(|s| vk.verify(si_canon.as_bytes(), &s).is_ok())
                    .unwrap_or(false)
            }
            other => return Some(format!("unsupported signature {other}")),
        };
        if ok {
            return None;
        }
    }
    Some("signature does not verify".into())
}

// ---- the response ----------------------------------------------------------------

/// What a valid response said.
#[derive(Debug, Clone)]
pub struct Accepted {
    pub name_id: String,
    pub name_id_format: Option<String>,
    pub session_index: Option<String>,
    pub session_not_on_or_after: Option<i64>,
    pub attributes: BTreeMap<String, Vec<String>>,
}

fn parse_instant(s: &str) -> Option<i64> {
    // 2026-09-02T19:04:32Z, with optional fraction
    let s = s.trim().trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) =
        (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    let time = time.split(['+', '-']).next().unwrap_or(time);
    let mut t = time.split(':');
    let (h, mi): (i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    let sec: f64 = t.next()?.parse().ok()?;
    // days from civil
    let (yy, mm) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * mm + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + sec as i64)
}

/// Check a base64 SAML response the way the plugin's validator checks it.
pub fn validate(
    response_b64: &str,
    request_id: Option<&str>,
    acs: &str,
    sp_entity_id: &str,
    idp_entity_id: &str,
    certs: &[Vec<u8>],
    now: i64,
) -> Result<Accepted, String> {
    use base64::Engine;
    let cleaned: String = response_b64.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&cleaned)
        .map_err(|_| "SAMLResponse cannot be decoded from base64".to_string())?;
    let xml = String::from_utf8_lossy(&bytes).to_string();
    let root = parse(&xml).ok_or_else(|| "SAMLResponse is not XML".to_string())?;
    if root.local != "Response" {
        return Err("not a samlp:Response".into());
    }
    // status
    let status_ok = root
        .child("Status")
        .and_then(|s| s.child("StatusCode"))
        .and_then(|c| c.attr("Value"))
        .map(|v| v == "urn:oasis:names:tc:SAML:2.0:status:Success")
        .unwrap_or(false);
    if !status_ok {
        return Err("status is not Success".into());
    }
    // destination, in-response-to, issuer
    if let Some(dest) = root.attr("Destination") {
        if dest != acs {
            return Err(format!("The response was received at {acs} instead of {dest}"));
        }
    }
    match (root.attr("InResponseTo"), request_id) {
        (Some(irt), Some(req)) if irt != req => {
            return Err("InResponseTo does not match the request".into());
        }
        (Some(_), None) => return Err("unsolicited response with InResponseTo".into()),
        (None, Some(_)) => return Err("the response has no InResponseTo".into()),
        _ => {}
    }
    if let Some(iss) = root.child("Issuer") {
        if iss.text().trim() != idp_entity_id {
            return Err("issuer is not the IdP".into());
        }
    }
    let assertion = root.child("Assertion").ok_or_else(|| "no assertion".to_string())?;
    if assertion.child("EncryptedAssertion").is_some() || root.child("EncryptedAssertion").is_some()
    {
        return Err("encrypted assertions are not supported".into());
    }
    if let Some(iss) = assertion.child("Issuer") {
        if iss.text().trim() != idp_entity_id {
            return Err("assertion issuer is not the IdP".into());
        }
    }
    // signatures: the response's, the assertion's, or both; at least one
    let mut any = false;
    for (owner, sig) in
        [(&root, root.child("Signature")), (assertion, assertion.child("Signature"))]
    {
        let _ = owner;
        if let Some(sig) = sig {
            if let Some(why) = verify_signature(&root, sig, certs) {
                return Err(format!("invalid signature: {why}"));
            }
            any = true;
        }
    }
    if !any {
        return Err("neither the response nor the assertion is signed".into());
    }
    // conditions
    if let Some(cond) = assertion.child("Conditions") {
        if let Some(nb) = cond.attr("NotBefore").and_then(parse_instant) {
            if now < nb {
                return Err("assertion is not yet valid".into());
            }
        }
        if let Some(na) = cond.attr("NotOnOrAfter").and_then(parse_instant) {
            if now >= na {
                return Err("assertion has expired".into());
            }
        }
        let audiences: Vec<String> = cond
            .children_named("AudienceRestriction")
            .flat_map(|ar| {
                ar.children_named("Audience")
                    .map(|a| a.text().trim().to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        if !audiences.is_empty() && !audiences.iter().any(|a| a == sp_entity_id) {
            return Err(format!("{sp_entity_id} is not a valid audience for this response"));
        }
    }
    // subject
    let subject = assertion.child("Subject").ok_or_else(|| "no subject".to_string())?;
    let name_id_node = subject.child("NameID").ok_or_else(|| "no NameID".to_string())?;
    let name_id = name_id_node.text().trim().to_string();
    let name_id_format = name_id_node.attr("Format").map(|s| s.to_string());
    for sc in subject.children_named("SubjectConfirmation") {
        if let Some(data) = sc.child("SubjectConfirmationData") {
            if let Some(r) = data.attr("Recipient") {
                if r != acs {
                    return Err("subject confirmation recipient is not the ACS".into());
                }
            }
            if let Some(na) = data.attr("NotOnOrAfter").and_then(parse_instant) {
                if now >= na {
                    return Err("subject confirmation has expired".into());
                }
            }
            if let (Some(irt), Some(req)) = (data.attr("InResponseTo"), request_id) {
                if irt != req {
                    return Err("subject confirmation InResponseTo does not match".into());
                }
            }
        }
    }
    let (session_index, session_not_on_or_after) = assertion
        .child("AuthnStatement")
        .map(|a| {
            (
                a.attr("SessionIndex").map(|s| s.to_string()),
                a.attr("SessionNotOnOrAfter").and_then(parse_instant),
            )
        })
        .unwrap_or((None, None));
    let mut attributes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for stmt in assertion.children_named("AttributeStatement") {
        for attr in stmt.children_named("Attribute") {
            let name = attr.attr("Name").unwrap_or("").to_string();
            let values: Vec<String> = attr
                .children_named("AttributeValue")
                .map(|v| v.text().trim().to_string())
                .collect();
            attributes.entry(name).or_default().extend(values);
        }
    }
    Ok(Accepted { name_id, name_id_format, session_index, session_not_on_or_after, attributes })
}

// ---- the settings and the flows ---------------------------------------------------

#[derive(Clone, Debug)]
pub struct SamlSettings {
    pub idp_entity_id: String,
    pub sp_entity_id: String,
    pub sso_url: String,
    pub sso_binding: String,
    pub slo_url: Option<String>,
    pub acs: String,
    pub certs: Vec<Vec<u8>>,
    pub roles_key: Option<String>,
    pub subject_key: Option<String>,
    pub roles_separator: Option<String>,
    /// the exchange key, padded to 64 bytes as the plugin pads it
    pub exchange_key: Vec<u8>,
    pub jwt_roles_key: String,
    pub jwt_subject_key: String,
    pub expiry_base: ExpiryBase,
    pub expiry_offset: i64,
    pub force_authn: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ExpiryBase {
    Auto,
    Now,
    Session,
}

/// The plugin's `padSecret`: a secret shorter than the digest is padded
/// with NULs to the digest's length.
pub fn pad_secret(secret: &[u8]) -> Vec<u8> {
    let mut v = secret.to_vec();
    while v.len() < 64 {
        v.push(0);
    }
    v
}

impl SamlSettings {
    /// The settings of a `saml` authenticator, its metadata read from a
    /// file, a URL or the config itself.
    pub fn from_config(cfg: &Value) -> Option<SamlSettings> {
        let text = |k: &str| -> Option<String> {
            cfg.pointer(&format!("/{}", k.replace('.', "/")))
                .or_else(|| cfg.get(k))
                .and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    Value::Null => None,
                    o => Some(o.to_string()),
                })
                .filter(|s| !s.is_empty())
        };
        let metadata_xml = if let Some(c) = text("idp.metadata_content") {
            c
        } else if let Some(f) = text("idp.metadata_file") {
            let path = std::path::PathBuf::from(&f);
            let path = if path.is_absolute() { path } else { crate::tls::config_dir().join(path) };
            std::fs::read_to_string(path).ok()?
        } else if let Some(u) = text("idp.metadata_url") {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(std::time::Duration::from_secs(10)))
                .build()
                .into();
            agent.get(&u).call().ok()?.body_mut().read_to_string().ok()?
        } else {
            return None;
        };
        let md = parse(&metadata_xml)?;
        let entity = md.attr("entityID").map(|s| s.to_string());
        let idp = md.find_first("IDPSSODescriptor")?;
        let mut sso: Option<(String, String)> = None;
        for s in idp.children_named("SingleSignOnService") {
            let b = s.attr("Binding").unwrap_or("").to_string();
            let l = s.attr("Location").unwrap_or("").to_string();
            if b.ends_with("HTTP-Redirect") || sso.is_none() {
                sso = Some((l, b));
            }
        }
        let (sso_url, sso_binding) = sso?;
        let slo_url = idp
            .children_named("SingleLogoutService")
            .find(|s| s.attr("Binding").unwrap_or("").ends_with("HTTP-Redirect"))
            .or_else(|| idp.children_named("SingleLogoutService").next())
            .and_then(|s| s.attr("Location").map(|l| l.to_string()));
        let kibana = text("kibana_url").unwrap_or_default();
        let acs = if kibana.ends_with('/') {
            format!("{kibana}_opendistro/_security/saml/acs")
        } else {
            format!("{kibana}/_opendistro/_security/saml/acs")
        };
        use base64::Engine;
        let exchange_key = text("exchange_key")
            .and_then(|k| {
                base64::engine::general_purpose::URL_SAFE
                    .decode(k.as_bytes())
                    .or_else(|_| {
                        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(k.as_bytes())
                    })
                    .ok()
            })
            .or_else(|| {
                text("jwt.key.k").and_then(|k| {
                    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(k.as_bytes()).ok()
                })
            })?;
        let (expiry_base, expiry_offset) = match text("jwt.expiry") {
            Some(e) => {
                let e = e.trim().to_string();
                let re = regex::Regex::new(r"^\s*(auto|now|session)?\s*([+-]?\d+)?\s*$").unwrap();
                match re.captures(&e.to_lowercase()) {
                    Some(c) => (
                        match c.get(1).map(|m| m.as_str()) {
                            Some("now") => ExpiryBase::Now,
                            Some("session") => ExpiryBase::Session,
                            _ => ExpiryBase::Auto,
                        },
                        c.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0),
                    ),
                    None => (ExpiryBase::Auto, 0),
                }
            }
            None => (ExpiryBase::Auto, 0),
        };
        Some(SamlSettings {
            idp_entity_id: text("idp.entity_id").or(entity).unwrap_or_default(),
            sp_entity_id: text("sp.entity_id").unwrap_or_default(),
            sso_url,
            sso_binding,
            slo_url,
            acs,
            certs: signing_certs(&md),
            roles_key: text("roles_key"),
            subject_key: text("subject_key"),
            roles_separator: text("roles_separator").or_else(|| text("roles_seperator")),
            exchange_key: pad_secret(&exchange_key),
            jwt_roles_key: text("jwt.roles_key").unwrap_or_else(|| "roles".into()),
            jwt_subject_key: text("jwt.subject_key").unwrap_or_else(|| "sub".into()),
            expiry_base,
            expiry_offset,
            force_authn: cfg.pointer("/sp/forceAuthn").and_then(|v| v.as_bool()),
        })
    }

    fn deflate_b64(xml: &str) -> String {
        use std::io::Write as _;
        let mut enc =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        let _ = enc.write_all(xml.as_bytes());
        let bytes = enc.finish().unwrap_or_default();
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn url_encode(s: &str) -> String {
        percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC)
            .to_string()
            .replace("%2A", "*")
            .replace("%2D", "-")
            .replace("%2E", ".")
            .replace("%5F", "_")
    }

    fn now_instant() -> String {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        crate::security::api::rfc3339_no_millis(secs)
    }

    /// The challenge: where the browser is sent, with the request's id.
    pub fn challenge(&self) -> (String, String) {
        let id = format!("ONELOGIN_{}", uuid_v4());
        let force = match self.force_authn {
            Some(true) => " ForceAuthn=\"true\"",
            _ => "",
        };
        let xml = format!(
            "<samlp:AuthnRequest xmlns:samlp=\"urn:oasis:names:tc:SAML:2.0:protocol\" xmlns:saml=\"urn:oasis:names:tc:SAML:2.0:assertion\" ID=\"{id}\" Version=\"2.0\" IssueInstant=\"{}\" Destination=\"{}\"{force} ProtocolBinding=\"urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST\" AssertionConsumerServiceURL=\"{}\"><saml:Issuer>{}</saml:Issuer><samlp:NameIDPolicy Format=\"urn:oasis:names:tc:SAML:1.1:nameid-format:unspecified\" AllowCreate=\"true\" /></samlp:AuthnRequest>",
            Self::now_instant(),
            self.sso_url,
            self.acs,
            self.sp_entity_id
        );
        let sep = if self.sso_url.contains('?') { "&" } else { "?" };
        let location = format!(
            "{}{sep}SAMLRequest={}",
            self.sso_url,
            Self::url_encode(&Self::deflate_b64(&xml))
        );
        (
            format!(
                "X-Security-IdP realm=\"OpenSearch Security\" location=\"{location}\" requestId=\"{id}\""
            ),
            id,
        )
    }

    /// The single-logout redirect for a caller that came through SAML.
    pub fn logout_url(
        &self,
        name_id: &str,
        name_id_format: Option<&str>,
        session_index: Option<&str>,
    ) -> Option<String> {
        let slo = self.slo_url.as_ref()?;
        let id = format!("ONELOGIN_{}", uuid_v4());
        let fmt = match name_id_format {
            Some(f) => format!(" Format=\"{}\"", long_format(f)),
            None => String::new(),
        };
        let si = session_index
            .map(|s| format!("<samlp:SessionIndex>{s}</samlp:SessionIndex>"))
            .unwrap_or_default();
        let xml = format!(
            "<samlp:LogoutRequest xmlns:samlp=\"urn:oasis:names:tc:SAML:2.0:protocol\" xmlns:saml=\"urn:oasis:names:tc:SAML:2.0:assertion\" ID=\"{id}\" Version=\"2.0\" IssueInstant=\"{}\" Destination=\"{slo}\"><saml:Issuer>{}</saml:Issuer><saml:NameID{fmt}>{name_id}</saml:NameID>{si}</samlp:LogoutRequest>",
            Self::now_instant(),
            self.sp_entity_id
        );
        let sep = if slo.contains('?') { "&" } else { "?" };
        Some(format!("{slo}{sep}SAMLRequest={}", Self::url_encode(&Self::deflate_b64(&xml))))
    }

    /// The token exchange: a response in, a JWT out.
    pub fn exchange(&self, body: &Value) -> Result<String, (u16, String)> {
        let Some(resp) = body.get("SAMLResponse").and_then(|v| v.as_str()) else {
            return Err((400, "SAMLResponse is missing from request".into()));
        };
        let request_id = body.get("RequestId").and_then(|v| v.as_str());
        let acs = match body.get("acsEndpoint").and_then(|v| v.as_str()) {
            Some(a) if a.starts_with("http://") || a.starts_with("https://") => a.to_string(),
            Some(a) => {
                let base = self.acs.trim_end_matches("/_opendistro/_security/saml/acs");
                format!(
                    "{base}{}",
                    if a.starts_with('/') { a.to_string() } else { format!("/{a}") }
                )
            }
            None => self.acs.clone(),
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let accepted = validate(
            resp,
            request_id,
            &acs,
            &self.sp_entity_id,
            &self.idp_entity_id,
            &self.certs,
            now,
        )
        .map_err(|why| (401, why))?;
        let subject = match &self.subject_key {
            Some(k) => accepted
                .attributes
                .get(k)
                .and_then(|v| v.first().cloned())
                .ok_or((401, "no subject".to_string()))?,
            None => accepted.name_id.clone(),
        };
        let roles: Option<Vec<String>> = self.roles_key.as_ref().map(|k| {
            let values = accepted.attributes.get(k).cloned().unwrap_or_default();
            match &self.roles_separator {
                Some(sep) => values
                    .iter()
                    .flat_map(|v| v.split(sep.as_str()).map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .collect(),
                None => values,
            }
        });
        let exp = match self.expiry_base {
            ExpiryBase::Now => now + self.expiry_offset,
            ExpiryBase::Session => match accepted.session_not_on_or_after {
                Some(s) => s + self.expiry_offset,
                None => now + if self.expiry_offset > 0 { self.expiry_offset } else { 3600 },
            },
            ExpiryBase::Auto => match accepted.session_not_on_or_after {
                Some(s) => s,
                None => now + if self.expiry_offset > 0 { self.expiry_offset } else { 3600 },
            },
        };
        let mut claims = serde_json::Map::new();
        claims.insert("nbf".into(), json!(now));
        claims.insert("exp".into(), json!(exp));
        claims.insert(self.jwt_subject_key.clone(), json!(subject));
        if self.subject_key.is_some() {
            claims.insert("saml_ni".into(), json!(accepted.name_id));
        }
        if let Some(f) = &accepted.name_id_format {
            claims.insert("saml_nif".into(), json!(short_format(f)));
        }
        if let Some(si) = &accepted.session_index {
            claims.insert("saml_si".into(), json!(si));
        }
        if let Some(r) = roles {
            claims.insert(self.jwt_roles_key.clone(), json!(r));
        }
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS512);
        jsonwebtoken::encode(
            &header,
            &Value::Object(claims),
            &jsonwebtoken::EncodingKey::from_secret(&self.exchange_key),
        )
        .map_err(|e| (500, e.to_string()))
    }
}

/// The plugin's short names for NameID formats.
fn short_format(uri: &str) -> &'static str {
    match uri {
        "urn:oasis:names:tc:SAML:1.1:nameid-format:unspecified" => "u",
        "urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress" => "e",
        "urn:oasis:names:tc:SAML:2.0:nameid-format:persistent" => "p",
        "urn:oasis:names:tc:SAML:2.0:nameid-format:transient" => "t",
        "urn:oasis:names:tc:SAML:1.1:nameid-format:X509SubjectName" => "x",
        "urn:oasis:names:tc:SAML:1.1:nameid-format:WindowsDomainQualifiedName" => "w",
        "urn:oasis:names:tc:SAML:2.0:nameid-format:kerberos" => "k",
        "urn:oasis:names:tc:SAML:2.0:nameid-format:entity" => "n",
        "urn:oasis:names:tc:SAML:2.0:nameid-format:encrypted" => "c",
        _ => "u",
    }
}

fn long_format(short: &str) -> &'static str {
    match short {
        "e" => "urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress",
        "p" => "urn:oasis:names:tc:SAML:2.0:nameid-format:persistent",
        "t" => "urn:oasis:names:tc:SAML:2.0:nameid-format:transient",
        "x" => "urn:oasis:names:tc:SAML:1.1:nameid-format:X509SubjectName",
        "w" => "urn:oasis:names:tc:SAML:1.1:nameid-format:WindowsDomainQualifiedName",
        "k" => "urn:oasis:names:tc:SAML:2.0:nameid-format:kerberos",
        "n" => "urn:oasis:names:tc:SAML:2.0:nameid-format:entity",
        "c" => "urn:oasis:names:tc:SAML:2.0:nameid-format:encrypted",
        _ => "urn:oasis:names:tc:SAML:1.1:nameid-format:unspecified",
    }
}

fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    // enough entropy for a request id: time, pid and a counter, hashed
    use sha2::Digest as _;
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut h = sha2::Sha256::new();
    h.update(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            .to_le_bytes(),
    );
    h.update(std::process::id().to_le_bytes());
    h.update(N.fetch_add(1, std::sync::atomic::Ordering::Relaxed).to_le_bytes());
    b.copy_from_slice(&h.finalize()[..16]);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalises_like_exc_c14n() {
        let doc = parse(
            "<a xmlns=\"urn:x\" xmlns:b=\"urn:b\" z=\"1\" b:y=\"2\"><b:c>t&amp;</b:c><d/></a>",
        )
        .unwrap();
        let s = c14n_exclusive(&doc, &BTreeMap::new(), &[]);
        assert_eq!(
            s,
            "<a xmlns=\"urn:x\" xmlns:b=\"urn:b\" z=\"1\" b:y=\"2\"><b:c>t&amp;</b:c><d></d></a>"
        );
    }

    #[test]
    fn reads_instants() {
        assert_eq!(parse_instant("1970-01-02T00:00:00Z"), Some(86_400));
        assert_eq!(parse_instant("2026-09-02T19:04:32Z"), Some(1_788_375_872));
    }
}
