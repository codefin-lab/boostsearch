//! Who a request is from, by every way the plugin can tell.
//!
//! `config.yml` names authentication domains in `dynamic.authc`, each an
//! HTTP authenticator (basic, jwt, openid, proxy, clientcert) over a
//! backend (internal users, noop, ldap), tried in `order`; and
//! authorization backends in `dynamic.authz` (ldap) that add backend roles
//! to whoever was authenticated. This runs them the way
//! `BackendRegistry` runs them: the first domain whose authenticator finds
//! credentials and whose backend accepts them wins; a domain that finds
//! none and is marked `challenge` answers 401 with its challenge; when
//! nothing accepts, the challenge of the first challenging domain is sent.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Map, Value};

use super::{Caller, SecurityConfig, java_set_order, pattern_matches};

/// What one request presents: everything an authenticator may read.
pub struct Presented<'a> {
    pub headers: &'a axum::http::HeaderMap,
    pub query: &'a str,
    /// the TCP peer
    pub remote: String,
    /// the subject DN of the client certificate, if one was presented
    pub peer_dn: Option<String>,
}

impl Presented<'_> {
    fn header(&self, name: &str) -> Option<String> {
        self.headers.get(name).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
    }

    fn param(&self, name: &str) -> Option<String> {
        for pair in self.query.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            if k == name {
                return Some(
                    percent_encoding::percent_decode_str(v).decode_utf8_lossy().replace('+', " "),
                );
            }
        }
        None
    }
}

/// Credentials one authenticator drew out of a request.
#[derive(Clone, Debug, Default)]
pub struct Credentials {
    pub name: String,
    pub password: Option<String>,
    /// roles the token or the proxy carried
    pub backend_roles: Vec<String>,
    pub attributes: BTreeMap<String, String>,
}

impl std::fmt::Debug for OpenIdSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenIdSettings")
            .field("connect_url", &self.connect_url)
            .field("jwks_uri", &self.jwks_uri)
            .field("subject_key", &self.subject_key)
            .field("roles_key", &self.roles_key)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub enum Authenticator {
    Basic,
    Jwt(JwtSettings),
    OpenId(Arc<OpenIdSettings>),
    Proxy {
        user_header: Option<String>,
        roles_header: Option<String>,
        roles_separator: String,
    },
    ClientCert {
        username_attribute: Option<String>,
        roles_attribute: Option<String>,
    },
    /// SAML: the token the exchange minted is read as a JWT; the challenge
    /// sends the browser to the IdP
    Saml(Arc<super::saml::SamlSettings>, JwtSettings),
    /// a kind this build does not carry (kerberos)
    Unsupported(String),
}

#[derive(Clone, Debug)]
pub struct JwtSettings {
    pub keys: Vec<SigningKey>,
    pub header: String,
    pub url_parameter: Option<String>,
    pub subject_key: Option<String>,
    pub roles_key: Vec<String>,
    pub required_audience: Vec<String>,
    pub required_issuer: Option<String>,
    /// the `jwt` type honours no skew; the openid type does
    pub clock_skew: u64,
}

#[derive(Clone, Debug)]
pub enum SigningKey {
    Hmac(Vec<u8>),
    RsaPem(String),
    EcPem(String),
}

pub struct OpenIdSettings {
    pub connect_url: Option<String>,
    pub jwks_uri: Option<String>,
    pub subject_key: Option<String>,
    pub roles_key: Vec<String>,
    pub required_audience: Vec<String>,
    pub required_issuer: Option<String>,
    pub clock_skew: u64,
    pub header: String,
    pub url_parameter: Option<String>,
    pub request_timeout: Duration,
    pub refresh_rate_limit_count: u32,
    pub refresh_rate_limit_window: Duration,
    keys: Mutex<KeyCache>,
}

#[derive(Default)]
struct KeyCache {
    /// by kid; a key with no kid is under the empty string
    keys: HashMap<String, (jsonwebtoken::DecodingKey, jsonwebtoken::Algorithm)>,
    loaded: bool,
    refreshes: Vec<Instant>,
}

#[derive(Clone, Debug)]
pub enum Backend {
    Internal,
    Noop,
    Ldap(Arc<LdapSettings>),
}

#[derive(Clone, Debug)]
pub struct LdapSettings {
    pub hosts: Vec<String>,
    pub enable_ssl: bool,
    pub enable_start_tls: bool,
    pub verify_hostnames: bool,
    pub bind_dn: Option<String>,
    pub password: Option<String>,
    pub userbase: String,
    pub usersearch: String,
    pub username_attribute: Option<String>,
    // authorization
    pub rolebase: String,
    pub rolesearch: String,
    pub rolename: String,
    pub userrolename: Vec<String>,
    pub userroleattribute: Option<String>,
    pub resolve_nested_roles: bool,
    pub max_nested_depth: usize,
    pub rolesearch_enabled: bool,
    pub skip_users: Vec<String>,
    pub exclude_roles: Vec<String>,
    pub nested_role_filter: Vec<String>,
    pub connect_timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct Domain {
    pub name: String,
    pub order: i64,
    pub challenge: bool,
    pub authenticator: Authenticator,
    pub backend: Backend,
}

#[derive(Clone, Debug)]
pub struct Authorizer {
    pub name: String,
    pub ldap: Arc<LdapSettings>,
}

/// `dynamic.http.xff`: which peers are proxies whose `X-Forwarded-For` is
/// believed.
#[derive(Clone, Debug)]
pub struct Xff {
    pub enabled: bool,
    pub internal_proxies: regex::Regex,
    pub remote_ip_header: String,
}

/// Everything `config.yml` says about telling callers apart.
#[derive(Clone, Debug)]
pub struct AuthChain {
    pub domains: Vec<Domain>,
    pub authorizers: Vec<Authorizer>,
    pub xff: Xff,
    pub anonymous: bool,
}

// ---- reading config.yml ---------------------------------------------------------

fn text(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(other) => Some(other.to_string()),
    }
}

fn list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(|x| text(Some(x))).collect(),
        Some(Value::String(s)) => {
            s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
        }
        _ => Vec::new(),
    }
}

fn flag(v: Option<&Value>, default: bool) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => default,
    }
}

fn int(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

impl SigningKey {
    /// A PEM public key is read as RSA, then as EC; anything else is a
    /// base64 HMAC secret, as `KeyUtils` reads them.
    fn parse(text: &str) -> Option<SigningKey> {
        let t = text.trim();
        if t.contains("-----BEGIN") {
            let pem = t.to_string();
            if jsonwebtoken::DecodingKey::from_rsa_pem(pem.as_bytes()).is_ok() {
                return Some(SigningKey::RsaPem(pem));
            }
            if jsonwebtoken::DecodingKey::from_ec_pem(pem.as_bytes()).is_ok() {
                return Some(SigningKey::EcPem(pem));
            }
            return None;
        }
        use base64::Engine;
        let cleaned: String = t.chars().filter(|c| !c.is_whitespace()).collect();
        base64::engine::general_purpose::STANDARD
            .decode(&cleaned)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(&cleaned))
            .ok()
            .map(SigningKey::Hmac)
    }

    fn decoding(&self, alg: jsonwebtoken::Algorithm) -> Option<jsonwebtoken::DecodingKey> {
        use jsonwebtoken::Algorithm::*;
        match (self, alg) {
            // a secret shorter than the digest is refused, as RFC 7518 asks
            // and as the plugin's jjwt refuses it
            (SigningKey::Hmac(k), HS256) if k.len() >= 32 => {
                Some(jsonwebtoken::DecodingKey::from_secret(k))
            }
            (SigningKey::Hmac(k), HS384) if k.len() >= 48 => {
                Some(jsonwebtoken::DecodingKey::from_secret(k))
            }
            (SigningKey::Hmac(k), HS512) if k.len() >= 64 => {
                Some(jsonwebtoken::DecodingKey::from_secret(k))
            }
            (SigningKey::RsaPem(p), RS256 | RS384 | RS512 | PS256 | PS384 | PS512) => {
                jsonwebtoken::DecodingKey::from_rsa_pem(p.as_bytes()).ok()
            }
            (SigningKey::EcPem(p), ES256 | ES384) => {
                jsonwebtoken::DecodingKey::from_ec_pem(p.as_bytes()).ok()
            }
            _ => None,
        }
    }
}

fn ldap_settings(c: &Value) -> LdapSettings {
    LdapSettings {
        hosts: list(c.get("hosts")),
        enable_ssl: flag(c.get("enable_ssl"), false),
        enable_start_tls: flag(c.get("enable_start_tls"), false),
        verify_hostnames: flag(c.get("verify_hostnames"), true),
        bind_dn: text(c.get("bind_dn")).filter(|s| !s.is_empty()),
        password: text(c.get("password")).filter(|s| !s.is_empty()),
        userbase: text(c.get("userbase")).unwrap_or_default(),
        usersearch: text(c.get("usersearch")).unwrap_or_else(|| "(sAMAccountName={0})".into()),
        username_attribute: text(c.get("username_attribute")).filter(|s| !s.is_empty()),
        rolebase: text(c.get("rolebase")).unwrap_or_default(),
        rolesearch: text(c.get("rolesearch")).unwrap_or_else(|| "(member={0})".into()),
        rolename: text(c.get("rolename")).unwrap_or_else(|| "name".into()),
        userrolename: text(c.get("userrolename"))
            .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
            .unwrap_or_else(|| vec!["memberOf".into()]),
        userroleattribute: text(c.get("userroleattribute")).filter(|s| !s.is_empty()),
        resolve_nested_roles: flag(c.get("resolve_nested_roles"), false),
        max_nested_depth: int(c.get("max_nested_depth")).unwrap_or(30).max(0) as usize,
        rolesearch_enabled: flag(c.get("rolesearch_enabled"), true),
        skip_users: list(c.get("skip_users")),
        exclude_roles: list(c.get("exclude_roles")),
        nested_role_filter: list(c.get("nested_role_filter")),
        connect_timeout: Duration::from_millis(
            int(c.get("connect_timeout")).unwrap_or(5000).max(100) as u64,
        ),
    }
}

fn jwt_common(
    c: &Value,
) -> (String, Option<String>, Option<String>, Vec<String>, Vec<String>, Option<String>, u64) {
    (
        text(c.get("jwt_header")).unwrap_or_else(|| "Authorization".into()),
        text(c.get("jwt_url_parameter")).filter(|s| !s.is_empty()),
        text(c.get("subject_key")).filter(|s| !s.is_empty()),
        list(c.get("roles_key")),
        list(c.get("required_audience")),
        text(c.get("required_issuer")).filter(|s| !s.is_empty()),
        int(c.get("jwt_clock_skew_tolerance_seconds")).unwrap_or(30).max(0) as u64,
    )
}

impl AuthChain {
    /// The chain `config.yml` describes; with no `authc` at all, the
    /// plugin's default of basic auth over the internal users.
    pub fn from_dynamic(dynamic: &Value) -> AuthChain {
        let mut domains = Vec::new();
        if let Some(authc) = dynamic.pointer("/dynamic/authc").and_then(|a| a.as_object()) {
            for (name, d) in authc {
                if !flag(d.get("http_enabled"), true) {
                    continue;
                }
                let ha = d.get("http_authenticator").cloned().unwrap_or(Value::Null);
                let cfg = ha.get("config").cloned().unwrap_or(Value::Object(Map::new()));
                let kind = text(ha.get("type")).unwrap_or_else(|| "basic".into()).to_lowercase();
                let authenticator = match kind.as_str() {
                    "basic" => Authenticator::Basic,
                    "jwt" => {
                        let (
                            header,
                            url_parameter,
                            subject_key,
                            roles_key,
                            required_audience,
                            required_issuer,
                            _,
                        ) = jwt_common(&cfg);
                        Authenticator::Jwt(JwtSettings {
                            keys: list(cfg.get("signing_key"))
                                .iter()
                                .filter_map(|k| SigningKey::parse(k))
                                .collect(),
                            header,
                            url_parameter,
                            subject_key,
                            roles_key,
                            required_audience,
                            required_issuer,
                            clock_skew: 0,
                        })
                    }
                    "openid" => {
                        let (
                            header,
                            url_parameter,
                            subject_key,
                            roles_key,
                            required_audience,
                            required_issuer,
                            skew,
                        ) = jwt_common(&cfg);
                        Authenticator::OpenId(Arc::new(OpenIdSettings {
                            connect_url: text(cfg.get("openid_connect_url"))
                                .filter(|s| !s.is_empty()),
                            jwks_uri: text(cfg.get("jwks_uri")).filter(|s| !s.is_empty()),
                            subject_key,
                            roles_key,
                            required_audience,
                            required_issuer,
                            clock_skew: skew,
                            header,
                            url_parameter,
                            request_timeout: Duration::from_millis(
                                int(cfg.get("idp_request_timeout_ms")).unwrap_or(5000).max(100)
                                    as u64,
                            ),
                            refresh_rate_limit_count: int(cfg.get("refresh_rate_limit_count"))
                                .unwrap_or(10)
                                .max(1)
                                as u32,
                            refresh_rate_limit_window: Duration::from_millis(
                                int(cfg.get("refresh_rate_limit_time_window_ms"))
                                    .unwrap_or(10_000)
                                    .max(1) as u64,
                            ),
                            keys: Mutex::new(KeyCache::default()),
                        }))
                    }
                    "proxy" | "extended-proxy" => Authenticator::Proxy {
                        user_header: text(cfg.get("user_header")).filter(|s| !s.is_empty()),
                        roles_header: text(cfg.get("roles_header")).filter(|s| !s.is_empty()),
                        roles_separator: text(cfg.get("roles_separator"))
                            .unwrap_or_else(|| ",".into()),
                    },
                    "saml" => match super::saml::SamlSettings::from_config(&cfg) {
                        Some(saml) => {
                            let jwt = JwtSettings {
                                keys: vec![SigningKey::Hmac(saml.exchange_key.clone())],
                                header: "Authorization".into(),
                                url_parameter: None,
                                subject_key: Some(saml.jwt_subject_key.clone()),
                                roles_key: vec![saml.jwt_roles_key.clone()],
                                required_audience: Vec::new(),
                                required_issuer: None,
                                clock_skew:
                                    int(cfg.pointer("/jwt/jwt_clock_skew_tolerance_seconds"))
                                        .unwrap_or(0)
                                        .max(0) as u64,
                            };
                            Authenticator::Saml(Arc::new(saml), jwt)
                        }
                        None => Authenticator::Unsupported("saml (metadata unreadable)".into()),
                    },
                    "clientcert" => Authenticator::ClientCert {
                        username_attribute: text(cfg.get("username_attribute"))
                            .filter(|s| !s.is_empty()),
                        roles_attribute: text(cfg.get("roles_attribute")).filter(|s| !s.is_empty()),
                    },
                    other => Authenticator::Unsupported(other.to_string()),
                };
                let be = d.get("authentication_backend").cloned().unwrap_or(Value::Null);
                let backend = match text(be.get("type"))
                    .unwrap_or_else(|| "internal".into())
                    .to_lowercase()
                    .as_str()
                {
                    "internal" | "intern" => Backend::Internal,
                    "noop" => Backend::Noop,
                    "ldap" => Backend::Ldap(Arc::new(ldap_settings(
                        &be.get("config").cloned().unwrap_or(Value::Null),
                    ))),
                    _ => Backend::Noop,
                };
                domains.push(Domain {
                    name: name.clone(),
                    order: int(d.get("order")).unwrap_or(i64::MAX),
                    challenge: flag(ha.get("challenge"), true),
                    authenticator,
                    backend,
                });
            }
        }
        domains.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.name.cmp(&b.name)));
        if domains.is_empty() {
            domains.push(Domain {
                name: "basic_internal_auth_domain".into(),
                order: 0,
                challenge: true,
                authenticator: Authenticator::Basic,
                backend: Backend::Internal,
            });
        }
        let mut authorizers = Vec::new();
        if let Some(authz) = dynamic.pointer("/dynamic/authz").and_then(|a| a.as_object()) {
            for (name, d) in authz {
                if !flag(d.get("http_enabled"), true) {
                    continue;
                }
                let be = d.get("authorization_backend").cloned().unwrap_or(Value::Null);
                if text(be.get("type")).unwrap_or_default().to_lowercase() == "ldap" {
                    authorizers.push(Authorizer {
                        name: name.clone(),
                        ldap: Arc::new(ldap_settings(
                            &be.get("config").cloned().unwrap_or(Value::Null),
                        )),
                    });
                }
            }
        }
        let xff_node = dynamic.pointer("/dynamic/http/xff").cloned().unwrap_or(Value::Null);
        let default_proxies = r"10\.\d{1,3}\.\d{1,3}\.\d{1,3}|192\.168\.\d{1,3}\.\d{1,3}|169\.254\.\d{1,3}\.\d{1,3}|127\.\d{1,3}\.\d{1,3}\.\d{1,3}|172\.1[6-9]{1}\.\d{1,3}\.\d{1,3}|172\.2[0-9]{1}\.\d{1,3}\.\d{1,3}|172\.3[0-1]{1}\.\d{1,3}\.\d{1,3}";
        let proxies =
            text(xff_node.get("internalProxies")).unwrap_or_else(|| default_proxies.to_string());
        let xff = Xff {
            enabled: flag(xff_node.get("enabled"), false),
            internal_proxies: regex::Regex::new(&format!("^(?:{proxies})$"))
                .unwrap_or_else(|_| regex::Regex::new("^$").unwrap()),
            remote_ip_header: text(xff_node.get("remoteIpHeader"))
                .unwrap_or_else(|| "X-Forwarded-For".into()),
        };
        AuthChain {
            domains,
            authorizers,
            xff,
            anonymous: dynamic
                .pointer("/dynamic/http/anonymous_auth_enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }
    }
}

// ---- resolving the peer through proxies -------------------------------------------

/// Where the request is really from: through the proxies `xff` trusts, or
/// the TCP peer itself. The second value says whether a proxy was trusted,
/// which is what lets the proxy authenticator run.
pub fn resolve_remote(xff: &Xff, presented: &Presented<'_>) -> (String, bool) {
    let peer = presented.remote.clone();
    if !xff.enabled || !xff.internal_proxies.is_match(&peer) {
        return (peer, false);
    }
    // the plugin marks the request as resolved through a proxy only once a
    // forwarded header was read; a trusted peer without one is just a peer
    let Some(chain) = presented.header(&xff.remote_ip_header) else { return (peer, false) };
    let hops: Vec<String> =
        chain.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    // walking from the right, the first address that is not a proxy is the client
    for hop in hops.iter().rev() {
        if !xff.internal_proxies.is_match(hop) {
            return (hop.clone(), true);
        }
    }
    (hops.first().cloned().unwrap_or(peer), true)
}

// ---- credentials ------------------------------------------------------------------

fn bearer_token(header_value: &str) -> String {
    let lower = header_value.to_lowercase();
    match lower.find("bearer ") {
        Some(i) => header_value[i + 7..].trim().to_string(),
        None => header_value.trim().to_string(),
    }
}

fn claims_subject(claims: &Value, subject_key: Option<&str>) -> Option<String> {
    let key = subject_key.unwrap_or("sub");
    match claims.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(other) => Some(other.to_string().trim_matches('"').to_string()),
    }
}

/// The roles a claim carries: a string is split on commas, a list is taken
/// as it is, anything else is written out and split.
fn claims_roles(claims: &Value, roles_key: &[String]) -> Vec<String> {
    for key in roles_key {
        let v = key.split('.').try_fold(claims, |cur, part| cur.get(part));
        match v {
            Some(Value::Array(a)) => {
                return a
                    .iter()
                    .map(|x| match x {
                        Value::String(s) => s.trim().to_string(),
                        other => other.to_string().trim_matches('"').to_string(),
                    })
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            Some(Value::Null) | None => continue,
            Some(Value::String(s)) => {
                return s
                    .split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect();
            }
            Some(other) => {
                return other
                    .to_string()
                    .split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect();
            }
        }
    }
    Vec::new()
}

fn jwt_validation(
    alg: jsonwebtoken::Algorithm,
    audience: &[String],
    issuer: Option<&str>,
    skew: u64,
) -> jsonwebtoken::Validation {
    let mut v = jsonwebtoken::Validation::new(alg);
    v.leeway = skew;
    v.validate_exp = true;
    v.validate_nbf = true;
    v.required_spec_claims.clear();
    if audience.is_empty() {
        v.validate_aud = false;
    } else {
        v.set_audience(audience);
    }
    if let Some(iss) = issuer {
        v.set_issuer(&[iss]);
    }
    v
}

impl JwtSettings {
    fn token(&self, p: &Presented<'_>) -> Option<String> {
        let mut token = p.header(&self.header);
        if let Some(param) = &self.url_parameter {
            if token.is_none() {
                token = p.param(param);
            }
        }
        let token = token?;
        let t = bearer_token(&token);
        if t.is_empty() { None } else { Some(t) }
    }

    /// The claims of a token one of the keys signed, else nothing.
    fn verify(&self, token: &str) -> Option<Value> {
        let header = jsonwebtoken::decode_header(token).ok()?;
        for key in &self.keys {
            let Some(dk) = key.decoding(header.alg) else { continue };
            let validation = jwt_validation(
                header.alg,
                &self.required_audience,
                self.required_issuer.as_deref(),
                self.clock_skew,
            );
            if let Ok(data) = jsonwebtoken::decode::<Value>(token, &dk, &validation) {
                return Some(data.claims);
            }
        }
        None
    }

    fn credentials(&self, p: &Presented<'_>) -> Option<Credentials> {
        let token = self.token(p)?;
        let claims = self.verify(&token)?;
        let name = claims_subject(&claims, self.subject_key.as_deref())?;
        let roles = claims_roles(&claims, &self.roles_key);
        let mut attributes = BTreeMap::new();
        if let Some(o) = claims.as_object() {
            for (k, v) in o {
                attributes
                    .insert(format!("attr.jwt.{k}"), v.to_string().trim_matches('"').to_string());
            }
        }
        Some(Credentials { name, password: None, backend_roles: roles, attributes })
    }
}

impl OpenIdSettings {
    fn token(&self, p: &Presented<'_>) -> Option<String> {
        let mut token = p.header(&self.header);
        if let Some(param) = &self.url_parameter {
            if token.is_none() {
                token = p.param(param);
            }
        }
        let t = bearer_token(&token?);
        if t.is_empty() { None } else { Some(t) }
    }

    /// The JWKS, from `jwks_uri` or through the discovery document.
    fn fetch_keys(
        &self,
    ) -> Option<Vec<(String, jsonwebtoken::DecodingKey, jsonwebtoken::Algorithm)>> {
        match self.fetch_keys_inner() {
            Ok(k) => Some(k),
            Err(e) => {
                if std::env::var("BOOSTSEARCH_AUTH_DEBUG").is_ok() {
                    eprintln!("oidc: fetch: {e}");
                }
                None
            }
        }
    }

    fn fetch_keys_inner(
        &self,
    ) -> Result<Vec<(String, jsonwebtoken::DecodingKey, jsonwebtoken::Algorithm)>, String> {
        let agent: ureq::Agent =
            ureq::Agent::config_builder().timeout_global(Some(self.request_timeout)).build().into();
        let jwks_uri = match (&self.jwks_uri, &self.connect_url) {
            (Some(u), _) => u.clone(),
            (None, Some(disc)) => {
                let doc: Value = agent
                    .get(disc)
                    .call()
                    .map_err(|e| format!("discovery {disc}: {e}"))?
                    .body_mut()
                    .read_json()
                    .map_err(|e| format!("discovery body: {e}"))?;
                doc.get("jwks_uri")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "discovery has no jwks_uri".to_string())?
                    .to_string()
            }
            (None, None) => return Err("neither openid_connect_url nor jwks_uri".into()),
        };
        let jwks: Value = agent
            .get(&jwks_uri)
            .call()
            .map_err(|e| format!("jwks {jwks_uri}: {e}"))?
            .body_mut()
            .read_json()
            .map_err(|e| format!("jwks body: {e}"))?;
        let mut out = Vec::new();
        for k in jwks
            .get("keys")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "jwks has no keys".to_string())?
        {
            let kid = k.get("kid").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let kty = k.get("kty").and_then(|v| v.as_str()).unwrap_or("");
            let alg = k.get("alg").and_then(|v| v.as_str());
            match kty {
                "RSA" => {
                    let (Some(n), Some(e)) =
                        (k.get("n").and_then(|v| v.as_str()), k.get("e").and_then(|v| v.as_str()))
                    else {
                        continue;
                    };
                    if let Ok(dk) = jsonwebtoken::DecodingKey::from_rsa_components(n, e) {
                        let a = match alg {
                            Some("RS384") => jsonwebtoken::Algorithm::RS384,
                            Some("RS512") => jsonwebtoken::Algorithm::RS512,
                            Some("PS256") => jsonwebtoken::Algorithm::PS256,
                            Some("PS384") => jsonwebtoken::Algorithm::PS384,
                            Some("PS512") => jsonwebtoken::Algorithm::PS512,
                            _ => jsonwebtoken::Algorithm::RS256,
                        };
                        out.push((kid, dk, a));
                    }
                }
                "EC" => {
                    let (Some(x), Some(y)) =
                        (k.get("x").and_then(|v| v.as_str()), k.get("y").and_then(|v| v.as_str()))
                    else {
                        continue;
                    };
                    if let Ok(dk) = jsonwebtoken::DecodingKey::from_ec_components(x, y) {
                        let a = match k.get("crv").and_then(|v| v.as_str()) {
                            Some("P-384") => jsonwebtoken::Algorithm::ES384,
                            _ => jsonwebtoken::Algorithm::ES256,
                        };
                        out.push((kid, dk, a));
                    }
                }
                "oct" => {
                    if let Some(kv) = k.get("k").and_then(|v| v.as_str()) {
                        use base64::Engine;
                        if let Ok(secret) =
                            base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(kv)
                        {
                            let a = match alg {
                                Some("HS384") => jsonwebtoken::Algorithm::HS384,
                                Some("HS512") => jsonwebtoken::Algorithm::HS512,
                                _ => jsonwebtoken::Algorithm::HS256,
                            };
                            out.push((kid, jsonwebtoken::DecodingKey::from_secret(&secret), a));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// A key by id, fetching the set when it is unknown; refreshes are
    /// rate-limited as the plugin's `SelfRefreshingKeySet` limits them.
    fn key(&self, kid: &str) -> Option<(jsonwebtoken::DecodingKey, jsonwebtoken::Algorithm)> {
        {
            let cache = self.keys.lock();
            if cache.loaded {
                if let Some(k) = cache.keys.get(kid) {
                    return Some(k.clone());
                }
                if kid.is_empty() && cache.keys.len() == 1 {
                    return cache.keys.values().next().cloned();
                }
            }
        }
        let allowed = {
            let mut cache = self.keys.lock();
            let now = Instant::now();
            cache.refreshes.retain(|t| now.duration_since(*t) < self.refresh_rate_limit_window);
            if cache.loaded && cache.refreshes.len() as u32 >= self.refresh_rate_limit_count {
                false
            } else {
                cache.refreshes.push(now);
                true
            }
        };
        if !allowed {
            return None;
        }
        let fetched = match self.fetch_keys() {
            Some(f) => f,
            None => {
                if std::env::var("BOOSTSEARCH_AUTH_DEBUG").is_ok() {
                    eprintln!(
                        "oidc: key set could not be fetched from {:?} / {:?}",
                        self.connect_url, self.jwks_uri
                    );
                }
                return None;
            }
        };
        let mut cache = self.keys.lock();
        cache.keys.clear();
        for (id, dk, alg) in fetched {
            cache.keys.insert(id, (dk, alg));
        }
        cache.loaded = true;
        if let Some(k) = cache.keys.get(kid) {
            return Some(k.clone());
        }
        if kid.is_empty() && cache.keys.len() == 1 {
            return cache.keys.values().next().cloned();
        }
        None
    }

    fn credentials(&self, p: &Presented<'_>) -> Option<Credentials> {
        let token = self.token(p)?;
        let debug = std::env::var("BOOSTSEARCH_AUTH_DEBUG").is_ok();
        let header = match jsonwebtoken::decode_header(&token) {
            Ok(h) => h,
            Err(e) => {
                if debug {
                    eprintln!("oidc: header: {e}");
                }
                return None;
            }
        };
        let kid = header.kid.clone().unwrap_or_default();
        let Some((dk, alg)) = self.key(&kid) else {
            if debug {
                eprintln!("oidc: no key for kid {kid:?}");
            }
            return None;
        };
        if alg != header.alg {
            if debug {
                eprintln!("oidc: alg {:?} vs key {:?}", header.alg, alg);
            }
            return None;
        }
        let validation = jwt_validation(
            alg,
            &self.required_audience,
            self.required_issuer.as_deref(),
            self.clock_skew,
        );
        let claims = match jsonwebtoken::decode::<Value>(&token, &dk, &validation) {
            Ok(d) => d.claims,
            Err(e) => {
                if debug {
                    eprintln!("oidc: decode: {e}");
                }
                return None;
            }
        };
        let name = claims_subject(&claims, self.subject_key.as_deref())?;
        let roles = claims_roles(&claims, &self.roles_key);
        let mut attributes = BTreeMap::new();
        if let Some(o) = claims.as_object() {
            for (k, v) in o {
                attributes
                    .insert(format!("attr.jwt.{k}"), v.to_string().trim_matches('"').to_string());
            }
        }
        Some(Credentials { name, password: None, backend_roles: roles, attributes })
    }
}

/// One attribute of an RFC 2253 DN, every value it has.
fn dn_attribute(dn: &str, attribute: &str) -> Vec<String> {
    let mut out = Vec::new();
    for rdn in dn.split(',') {
        for part in rdn.split('+') {
            if let Some((k, v)) = part.split_once('=') {
                if k.trim().eq_ignore_ascii_case(attribute) {
                    out.push(v.trim().to_string());
                }
            }
        }
    }
    out
}

fn basic_credentials(p: &Presented<'_>) -> Option<Credentials> {
    let h = p.header("authorization")?;
    let encoded = h.strip_prefix("Basic ").or_else(|| h.strip_prefix("basic "))?;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).ok()?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    let (name, password) = text.split_once(':')?;
    Some(Credentials {
        name: name.to_string(),
        password: Some(password.to_string()),
        ..Default::default()
    })
}

impl Authenticator {
    /// What this authenticator can read off the request, if anything.
    fn extract(&self, p: &Presented<'_>, through_proxy: bool) -> Option<Credentials> {
        match self {
            Authenticator::Basic => basic_credentials(p),
            Authenticator::Jwt(j) => j.credentials(p),
            Authenticator::OpenId(o) => o.credentials(p),
            Authenticator::Proxy { user_header, roles_header, roles_separator } => {
                // the plugin refuses to read proxy headers unless the peer is
                // a proxy `xff` trusts
                if !through_proxy {
                    return None;
                }
                let name = p.header(user_header.as_deref()?)?;
                if name.is_empty() {
                    return None;
                }
                let roles = roles_header
                    .as_deref()
                    .and_then(|h| p.header(h))
                    .map(|v| {
                        let sep = regex::Regex::new(roles_separator)
                            .unwrap_or_else(|_| regex::Regex::new(",").unwrap());
                        sep.split(&v)
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                Some(Credentials {
                    name,
                    password: None,
                    backend_roles: roles,
                    ..Default::default()
                })
            }
            Authenticator::ClientCert { username_attribute, roles_attribute } => {
                let dn = p.peer_dn.clone()?;
                let name = match username_attribute {
                    Some(attr) => dn_attribute(&dn, attr).into_iter().next()?,
                    None => dn.clone(),
                };
                let roles =
                    roles_attribute.as_deref().map(|a| dn_attribute(&dn, a)).unwrap_or_default();
                Some(Credentials {
                    name,
                    password: None,
                    backend_roles: roles,
                    ..Default::default()
                })
            }
            Authenticator::Saml(_, jwt) => jwt.credentials(p),
            Authenticator::Unsupported(_) => None,
        }
    }
}

// ---- LDAP ---------------------------------------------------------------------------

/// A value inside an LDAP filter, escaped as RFC 4515 asks.
fn ldap_escape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '*' => out.push_str("\\2a"),
            '(' => out.push_str("\\28"),
            ')' => out.push_str("\\29"),
            '\\' => out.push_str("\\5c"),
            '\0' => out.push_str("\\00"),
            other => out.push(other),
        }
    }
    out
}

fn fill(pattern: &str, zero: &str, one: &str, two: &str) -> String {
    pattern
        .replace("{0}", &ldap_escape(zero))
        .replace("{1}", &ldap_escape(one))
        .replace("{2}", &ldap_escape(two))
}

impl LdapSettings {
    fn url(&self) -> Option<String> {
        let host = self.hosts.first()?;
        if host.contains("://") {
            return Some(host.clone());
        }
        Some(if self.enable_ssl { format!("ldaps://{host}") } else { format!("ldap://{host}") })
    }

    async fn connect(&self) -> Option<ldap3::Ldap> {
        let url = self.url()?;
        let mut settings = ldap3::LdapConnSettings::new().set_conn_timeout(self.connect_timeout);
        if !self.verify_hostnames {
            settings = settings.set_no_tls_verify(true);
        }
        if self.enable_start_tls {
            settings = settings.set_starttls(true);
        }
        let (conn, mut ldap) = ldap3::LdapConnAsync::with_settings(settings, &url).await.ok()?;
        ldap3::drive!(conn);
        match (&self.bind_dn, &self.password) {
            (Some(dn), Some(pw)) => {
                ldap.simple_bind(dn, pw).await.ok()?.success().ok()?;
            }
            _ => {}
        }
        Some(ldap)
    }

    /// The user's entry: its DN and attributes.
    async fn find_user(&self, ldap: &mut ldap3::Ldap, name: &str) -> Option<ldap3::SearchEntry> {
        let filter = fill(&self.usersearch, name, "", "");
        let (rs, _) = ldap
            .search(&self.userbase, ldap3::Scope::Subtree, &filter, vec!["*", "+"])
            .await
            .ok()?
            .success()
            .ok()?;
        rs.into_iter().next().map(ldap3::SearchEntry::construct)
    }

    /// Bind as the user the way the plugin does: find the entry with the
    /// search account, then bind with the entry's DN and the password.
    pub async fn authenticate(&self, name: &str, password: &str) -> Option<Credentials> {
        if password.is_empty() {
            return None;
        }
        let mut ldap = self.connect().await?;
        let entry = self.find_user(&mut ldap, name).await?;
        let dn = entry.dn.clone();
        let mut user_conn = self.connect_unbound().await?;
        let bound = user_conn.simple_bind(&dn, password).await.ok()?.success().is_ok();
        let _ = user_conn.unbind().await;
        let _ = ldap.unbind().await;
        if !bound {
            return None;
        }
        let shown = match &self.username_attribute {
            Some(attr) => entry
                .attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(attr))
                .and_then(|(_, v)| v.first().cloned())
                .unwrap_or_else(|| name.to_string()),
            None => name.to_string(),
        };
        let mut attributes = BTreeMap::new();
        attributes.insert("ldap.dn".into(), dn.clone());
        attributes.insert("ldap.original.username".into(), name.to_string());
        for (k, v) in &entry.attrs {
            if let Some(first) = v.first() {
                attributes.insert(format!("attr.ldap.{k}"), first.clone());
            }
        }
        Some(Credentials { name: shown, password: None, backend_roles: Vec::new(), attributes })
    }

    async fn connect_unbound(&self) -> Option<ldap3::Ldap> {
        let url = self.url()?;
        let mut settings = ldap3::LdapConnSettings::new().set_conn_timeout(self.connect_timeout);
        if !self.verify_hostnames {
            settings = settings.set_no_tls_verify(true);
        }
        if self.enable_start_tls {
            settings = settings.set_starttls(true);
        }
        let (conn, ldap) = ldap3::LdapConnAsync::with_settings(settings, &url).await.ok()?;
        ldap3::drive!(conn);
        Some(ldap)
    }

    fn role_name_of(&self, entry: &ldap3::SearchEntry) -> Option<String> {
        if self.rolename.eq_ignore_ascii_case("dn") {
            return Some(entry.dn.clone());
        }
        entry
            .attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&self.rolename))
            .and_then(|(_, v)| v.first().cloned())
    }

    /// The roles LDAP holds for a user: the ones named on the entry
    /// (`userrolename`), the groups that name the user (`rolesearch`), and
    /// the groups those groups belong to when nesting is resolved.
    pub async fn roles_for(&self, name: &str, known_dn: Option<&str>) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let Some(mut ldap) = self.connect().await else { return out };
        let entry = match known_dn {
            Some(dn) => Some((dn.to_string(), Vec::<(String, Vec<String>)>::new())),
            None => self
                .find_user(&mut ldap, name)
                .await
                .map(|e| (e.dn.clone(), e.attrs.into_iter().collect())),
        };
        let Some((user_dn, attrs)) = entry else { return out };
        let mut role_dns: Vec<String> = Vec::new();
        // roles named on the user's own entry
        if !self.userrolename.iter().any(|n| n.eq_ignore_ascii_case("disabled")) {
            for attr in &self.userrolename {
                if let Some((_, values)) = attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(attr))
                {
                    for v in values {
                        if v.contains('=') {
                            role_dns.push(v.clone());
                        } else {
                            out.push(v.clone());
                        }
                    }
                }
            }
        }
        // groups that name the user
        if self.rolesearch_enabled && !self.rolebase.is_empty() {
            let two = self
                .userroleattribute
                .as_deref()
                .and_then(|a| attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(a)))
                .and_then(|(_, v)| v.first().cloned())
                .unwrap_or_default();
            let filter = fill(&self.rolesearch, &user_dn, name, &two);
            if let Ok(res) =
                ldap.search(&self.rolebase, ldap3::Scope::Subtree, &filter, vec!["*"]).await
            {
                if let Ok((rs, _)) = res.success() {
                    for r in rs {
                        let e = ldap3::SearchEntry::construct(r);
                        if !role_dns.contains(&e.dn) {
                            role_dns.push(e.dn.clone());
                        }
                        if let Some(n) = self.role_name_of(&e) {
                            out.push(n);
                        }
                    }
                }
            }
        }
        // roles named by DN on the entry need their names read
        let mut seen: HashSet<String> = role_dns.iter().cloned().collect();
        let mut frontier: Vec<String> = role_dns.clone();
        let mut depth = 0usize;
        while !frontier.is_empty() {
            let mut next = Vec::new();
            for dn in &frontier {
                if !self.rolename.eq_ignore_ascii_case("dn") && !out.iter().any(|_| false) {
                    // a DN from the user's entry: its name is read from the entry
                    if let Ok(res) =
                        ldap.search(dn, ldap3::Scope::Base, "(objectClass=*)", vec!["*"]).await
                    {
                        if let Ok((rs, _)) = res.success() {
                            if let Some(e) =
                                rs.into_iter().next().map(ldap3::SearchEntry::construct)
                            {
                                if let Some(n) = self.role_name_of(&e) {
                                    if !out.contains(&n) {
                                        out.push(n);
                                    }
                                }
                            }
                        }
                    }
                } else if self.rolename.eq_ignore_ascii_case("dn") && !out.contains(dn) {
                    out.push(dn.clone());
                }
                if self.resolve_nested_roles
                    && depth < self.max_nested_depth
                    && !self.rolebase.is_empty()
                {
                    let filter = if self.nested_role_filter.is_empty() {
                        fill(&self.rolesearch, dn, name, "")
                    } else {
                        fill(&self.rolesearch, dn, name, "")
                    };
                    if let Ok(res) =
                        ldap.search(&self.rolebase, ldap3::Scope::Subtree, &filter, vec!["*"]).await
                    {
                        if let Ok((rs, _)) = res.success() {
                            for r in rs {
                                let e = ldap3::SearchEntry::construct(r);
                                if self.nested_role_filter.iter().any(|p| pattern_matches(p, &e.dn))
                                {
                                    continue;
                                }
                                if seen.insert(e.dn.clone()) {
                                    if let Some(n) = self.role_name_of(&e) {
                                        if !out.contains(&n) {
                                            out.push(n);
                                        }
                                    }
                                    next.push(e.dn.clone());
                                }
                            }
                        }
                    }
                }
            }
            frontier = next;
            depth += 1;
        }
        let _ = ldap.unbind().await;
        out.retain(|r| !self.exclude_roles.iter().any(|p| pattern_matches(p, r)));
        let mut dedup = Vec::new();
        for r in out {
            if !dedup.contains(&r) {
                dedup.push(r);
            }
        }
        dedup
    }
}

// ---- running the chain ----------------------------------------------------------------

/// Why a request could not be authenticated, and what to answer.
#[derive(Debug)]
pub enum Refusal {
    /// a challenge: 401 with this `WWW-Authenticate`
    Challenge(String),
    /// nothing to challenge with: 403
    Forbidden,
}

/// A user the chain accepted once, kept for `cache.ttl_minutes` as the
/// plugin keeps them; a token's roles are added to the kept user, as the
/// plugin adds them.
#[derive(Clone)]
struct KeptUser {
    at: Instant,
    backend_roles: Vec<String>,
    attributes: BTreeMap<String, String>,
}

pub struct ChainState {
    kept: Mutex<HashMap<(String, String), KeptUser>>,
    ttl: Duration,
}

impl ChainState {
    pub fn new(ttl: Duration) -> ChainState {
        ChainState { kept: Mutex::new(HashMap::new()), ttl }
    }

    pub fn clear(&self) {
        self.kept.lock().clear();
    }
}

impl AuthChain {
    /// The caller a request stands for, or why it is refused.
    pub async fn authenticate(
        &self,
        cfg: &SecurityConfig,
        state: &ChainState,
        presented: &Presented<'_>,
    ) -> Result<Caller, Refusal> {
        let (remote, through_proxy) = resolve_remote(&self.xff, presented);
        let mut first_challenge: Option<String> = None;
        for domain in &self.domains {
            let creds = domain.authenticator.extract(presented, through_proxy);
            let Some(creds) = creds else {
                if domain.challenge {
                    let ch = challenge_of(&domain.authenticator);
                    if !self.anonymous {
                        return Err(Refusal::Challenge(ch));
                    }
                    if first_challenge.is_none() {
                        first_challenge = Some(ch);
                    }
                }
                continue;
            };
            if first_challenge.is_none() && domain.challenge {
                first_challenge = Some(challenge_of(&domain.authenticator));
            }
            // the backend
            let accepted: Option<Credentials> = match &domain.backend {
                Backend::Noop => Some(creds.clone()),
                Backend::Internal => {
                    let Some(pw) = creds.password.as_deref() else { continue };
                    match cfg.authenticate(&creds.name, pw) {
                        Some(user) => Some(Credentials {
                            name: creds.name.clone(),
                            password: None,
                            backend_roles: user.backend_roles.clone(),
                            attributes: user.attributes.clone(),
                        }),
                        None => None,
                    }
                }
                Backend::Ldap(l) => {
                    let Some(pw) = creds.password.as_deref() else { continue };
                    l.authenticate(&creds.name, pw).await
                }
            };
            let Some(mut user) = accepted else { continue };
            // what the internal store says about the user's own security roles
            let security_roles = if matches!(domain.backend, Backend::Internal) {
                cfg.users.get(&user.name).map(|u| u.security_roles.clone()).unwrap_or_default()
            } else {
                Vec::new()
            };
            // the plugin keeps the user and adds each token's roles to it
            let key = (domain.name.clone(), user.name.clone());
            let already: Option<KeptUser> = {
                let kept = state.kept.lock();
                kept.get(&key).filter(|k| Instant::now().duration_since(k.at) < state.ttl).cloned()
            };
            match already {
                Some(mut k) => {
                    for r in &user.backend_roles {
                        if !k.backend_roles.contains(r) {
                            k.backend_roles.push(r.clone());
                        }
                    }
                    for (a, v) in &user.attributes {
                        k.attributes.entry(a.clone()).or_insert_with(|| v.clone());
                    }
                    user.backend_roles = k.backend_roles.clone();
                    user.attributes = k.attributes.clone();
                    state.kept.lock().insert(key, k);
                }
                None => {
                    // the authorizers run once, when the user is first kept;
                    // no lock is held while they ask LDAP
                    for a in &self.authorizers {
                        if a.ldap.skip_users.iter().any(|p| pattern_matches(p, &user.name)) {
                            continue;
                        }
                        let dn = user.attributes.get("ldap.dn").cloned();
                        let roles = a.ldap.roles_for(&user.name, dn.as_deref()).await;
                        for r in roles {
                            if !user.backend_roles.contains(&r) {
                                user.backend_roles.push(r);
                            }
                        }
                    }
                    state.kept.lock().insert(
                        key,
                        KeptUser {
                            at: Instant::now(),
                            backend_roles: user.backend_roles.clone(),
                            attributes: user.attributes.clone(),
                        },
                    );
                }
            }
            let backend_roles = java_set_order(user.backend_roles.iter().cloned().collect());
            let roles = cfg.map_roles(&user.name, &backend_roles, &security_roles, &remote);
            return Ok(Caller {
                name: user.name,
                backend_roles,
                attributes: user.attributes,
                roles,
                remote_address: remote,
                is_internal: matches!(domain.backend, Backend::Internal),
                requested_tenant: None,
                unrestricted: false,
                admin_cert: false,
            });
        }
        if self.anonymous {
            let roles = cfg.map_roles(
                "opendistro_security_anonymous",
                &["opendistro_security_anonymous_backendrole".to_string()],
                &[],
                &remote,
            );
            return Ok(Caller {
                name: "opendistro_security_anonymous".into(),
                backend_roles: vec!["opendistro_security_anonymous_backendrole".into()],
                roles,
                remote_address: remote,
                ..Caller::default()
            });
        }
        match first_challenge.or_else(|| {
            self.domains.iter().find(|d| d.challenge).map(|d| challenge_of(&d.authenticator))
        }) {
            Some(ch) => Err(Refusal::Challenge(ch)),
            None => Err(Refusal::Forbidden),
        }
    }
}

/// The `WWW-Authenticate` an authenticator challenges with.
fn challenge_of(a: &Authenticator) -> String {
    match a {
        Authenticator::Basic => "Basic realm=\"OpenSearch Security\"".into(),
        Authenticator::Jwt(_) | Authenticator::OpenId(_) => {
            "Bearer realm=\"OpenSearch Security\"".into()
        }
        Authenticator::Saml(s, _) => s.challenge().0,
        _ => "Basic realm=\"OpenSearch Security\"".into(),
    }
}

impl AuthChain {
    /// The SAML domain, when there is one: the token exchange is its.
    pub fn saml(&self) -> Option<Arc<super::saml::SamlSettings>> {
        self.domains.iter().find_map(|d| match &d.authenticator {
            Authenticator::Saml(s, _) => Some(s.clone()),
            _ => None,
        })
    }
}
