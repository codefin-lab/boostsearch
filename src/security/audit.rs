//! The audit log: who did what, in the plugin's own records.
//!
//! Every category the plugin writes is written here with the same fields
//! (`audit_category`, `audit_request_layer`, `audit_rest_request_*`,
//! `audit_transport_request_type`, `audit_request_privilege`,
//! `audit_trace_*`, `audit_compliance_*`, `audit_node_*`, `@timestamp`,
//! `audit_format_version` 4), filtered the way `audit.yml` says
//! (`enabled`, disabled categories per layer, `ignore_users`,
//! `ignore_requests`, `ignore_headers`, `ignore_url_params`,
//! `exclude_sensitive_headers`, `log_request_body`, `resolve_indices`,
//! `resolve_bulk_requests`, and the compliance section), and delivered to
//! the sink `plugins.security.audit.type` names: the index
//! `security-auditlog-YYYY.MM.dd` inside this node, stderr, a webhook, or
//! another cluster over HTTP. Writing never waits on the sink: messages
//! go down a channel to one thread.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::{Map, Value, json};

use super::{Caller, pattern_matches};

// ---- the dynamic configuration (`audit.yml`) -------------------------------------

/// `config.audit`
#[derive(Clone, Debug)]
pub struct Filter {
    pub enable_rest: bool,
    pub enable_transport: bool,
    pub disabled_rest_categories: Vec<String>,
    pub disabled_transport_categories: Vec<String>,
    pub ignore_users: Vec<String>,
    pub ignore_requests: Vec<String>,
    pub ignore_headers: Vec<String>,
    pub ignore_url_params: Vec<String>,
    pub resolve_bulk_requests: bool,
    pub log_request_body: bool,
    pub resolve_indices: bool,
    pub exclude_sensitive_headers: bool,
}

/// `config.compliance`
#[derive(Clone, Debug)]
pub struct Compliance {
    pub enabled: bool,
    pub internal_config: bool,
    pub external_config: bool,
    pub read_metadata_only: bool,
    pub read_watched_fields: BTreeMap<String, Vec<String>>,
    pub read_ignore_users: Vec<String>,
    pub write_metadata_only: bool,
    pub write_log_diffs: bool,
    pub write_watched_indices: Vec<String>,
    pub write_ignore_users: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct AuditConfig {
    pub enabled: bool,
    pub audit: Filter,
    pub compliance: Compliance,
}

pub const REST_CATEGORIES: &[&str] = &[
    "BAD_HEADERS",
    "SSL_EXCEPTION",
    "AUTHENTICATED",
    "FAILED_LOGIN",
    "GRANTED_PRIVILEGES",
    "MISSING_PRIVILEGES",
];
pub const TRANSPORT_CATEGORIES: &[&str] = &[
    "BAD_HEADERS",
    "SSL_EXCEPTION",
    "AUTHENTICATED",
    "FAILED_LOGIN",
    "GRANTED_PRIVILEGES",
    "MISSING_PRIVILEGES",
    "INDEX_EVENT",
    "OPENDISTRO_SECURITY_INDEX_ATTEMPT",
];

fn flag(v: Option<&Value>, default: bool) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => default,
    }
}

fn list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => {
            a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()
        }
        _ => Vec::new(),
    }
}

impl AuditConfig {
    /// The configuration a `config` object describes; anything missing is
    /// the plugin's default.
    pub fn from_json(v: &Value) -> AuditConfig {
        let a = v.get("audit").cloned().unwrap_or(Value::Null);
        let c = v.get("compliance").cloned().unwrap_or(Value::Null);
        let default_disabled =
            || vec!["AUTHENTICATED".to_string(), "GRANTED_PRIVILEGES".to_string()];
        AuditConfig {
            enabled: flag(v.get("enabled"), true),
            audit: Filter {
                enable_rest: flag(a.get("enable_rest"), true),
                enable_transport: flag(a.get("enable_transport"), true),
                disabled_rest_categories: match a.get("disabled_rest_categories") {
                    Some(Value::Array(_)) => list(a.get("disabled_rest_categories")),
                    _ => default_disabled(),
                },
                disabled_transport_categories: match a.get("disabled_transport_categories") {
                    Some(Value::Array(_)) => list(a.get("disabled_transport_categories")),
                    _ => default_disabled(),
                },
                ignore_users: match a.get("ignore_users") {
                    Some(Value::Array(_)) => list(a.get("ignore_users")),
                    _ => vec!["kibanaserver".into()],
                },
                ignore_requests: list(a.get("ignore_requests")),
                ignore_headers: list(a.get("ignore_headers")),
                ignore_url_params: list(a.get("ignore_url_params")),
                resolve_bulk_requests: flag(a.get("resolve_bulk_requests"), false),
                log_request_body: flag(a.get("log_request_body"), true),
                resolve_indices: flag(a.get("resolve_indices"), true),
                exclude_sensitive_headers: flag(a.get("exclude_sensitive_headers"), true),
            },
            compliance: Compliance {
                enabled: flag(c.get("enabled"), true),
                internal_config: flag(c.get("internal_config"), true),
                external_config: flag(c.get("external_config"), false),
                read_metadata_only: flag(c.get("read_metadata_only"), true),
                read_watched_fields: c
                    .get("read_watched_fields")
                    .and_then(|m| m.as_object())
                    .map(|o| o.iter().map(|(k, v)| (k.clone(), list(Some(v)))).collect())
                    .unwrap_or_default(),
                read_ignore_users: match c.get("read_ignore_users") {
                    Some(Value::Array(_)) => list(c.get("read_ignore_users")),
                    _ => vec!["kibanaserver".into()],
                },
                write_metadata_only: flag(c.get("write_metadata_only"), true),
                write_log_diffs: flag(c.get("write_log_diffs"), false),
                write_watched_indices: list(c.get("write_watched_indices")),
                write_ignore_users: match c.get("write_ignore_users") {
                    Some(Value::Array(_)) => list(c.get("write_ignore_users")),
                    _ => vec!["kibanaserver".into()],
                },
            },
        }
    }

    /// The `config` object as the API reports it, keys in the plugin's order.
    pub fn to_json(&self) -> Value {
        json!({
            "compliance": {
                "enabled": self.compliance.enabled,
                "write_log_diffs": self.compliance.write_log_diffs,
                "read_watched_fields": self.compliance.read_watched_fields,
                "read_ignore_users": self.compliance.read_ignore_users,
                "write_watched_indices": self.compliance.write_watched_indices,
                "write_ignore_users": self.compliance.write_ignore_users,
                "read_metadata_only": self.compliance.read_metadata_only,
                "write_metadata_only": self.compliance.write_metadata_only,
                "external_config": self.compliance.external_config,
                "internal_config": self.compliance.internal_config,
            },
            "enabled": self.enabled,
            "audit": {
                "ignore_users": self.audit.ignore_users,
                "ignore_requests": self.audit.ignore_requests,
                "ignore_headers": self.audit.ignore_headers,
                "ignore_url_params": self.audit.ignore_url_params,
                "disabled_rest_categories": self.audit.disabled_rest_categories,
                "disabled_transport_categories": self.audit.disabled_transport_categories,
                "exclude_sensitive_headers": self.audit.exclude_sensitive_headers,
                "log_request_body": self.audit.log_request_body,
                "resolve_indices": self.audit.resolve_indices,
                "resolve_bulk_requests": self.audit.resolve_bulk_requests,
                "enable_transport": self.audit.enable_transport,
                "enable_rest": self.audit.enable_rest,
            },
        })
    }

    /// The keys a `config` may carry; anything else is refused as the
    /// plugin refuses it.
    pub fn validate(v: &Value) -> Result<(), String> {
        let Some(o) = v.as_object() else {
            return Err("Could not parse content of request.".into());
        };
        for k in o.keys() {
            if !["enabled", "audit", "compliance"].contains(&k.as_str()) {
                return Err("Could not parse content of request.".into());
            }
        }
        let audit_keys = [
            "enable_rest",
            "disabled_rest_categories",
            "enable_transport",
            "disabled_transport_categories",
            "ignore_users",
            "ignore_requests",
            "ignore_headers",
            "ignore_url_params",
            "resolve_bulk_requests",
            "log_request_body",
            "resolve_indices",
            "exclude_sensitive_headers",
        ];
        if let Some(a) = o.get("audit") {
            let Some(ao) = a.as_object() else {
                return Err("Could not parse content of request.".into());
            };
            for k in ao.keys() {
                if !audit_keys.contains(&k.as_str()) {
                    return Err("Could not parse content of request.".into());
                }
            }
            for c in list(ao.get("disabled_rest_categories")) {
                if !REST_CATEGORIES.contains(&c.as_str()) {
                    return Err("Could not parse content of request.".into());
                }
            }
            for c in list(ao.get("disabled_transport_categories")) {
                if !TRANSPORT_CATEGORIES.contains(&c.as_str()) {
                    return Err("Could not parse content of request.".into());
                }
            }
        }
        let compliance_keys = [
            "enabled",
            "internal_config",
            "external_config",
            "read_metadata_only",
            "read_watched_fields",
            "read_ignore_users",
            "write_metadata_only",
            "write_log_diffs",
            "write_watched_indices",
            "write_ignore_users",
        ];
        if let Some(c) = o.get("compliance") {
            let Some(co) = c.as_object() else {
                return Err("Could not parse content of request.".into());
            };
            for k in co.keys() {
                if !compliance_keys.contains(&k.as_str()) {
                    return Err("Could not parse content of request.".into());
                }
            }
        }
        Ok(())
    }
}

/// The plugin's shipped `audit.yml`, laid under the one on disk.
fn embedded_default() -> Value {
    let y: serde_yaml::Value =
        serde_yaml::from_str(include_str!("defaults/audit.yml")).unwrap_or_default();
    serde_json::to_value(y)
        .unwrap_or(Value::Null)
        .get("config")
        .cloned()
        .unwrap_or(Value::Object(Map::new()))
}

// ---- the sink -------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum Sink {
    /// an index inside this node, named by a Joda-style pattern
    Internal {
        index_pattern: String,
    },
    /// stderr, as the plugin's `debug` sink prints
    Debug,
    /// stderr under a logger name, as `log4j` would
    Log4j {
        logger: String,
        level: String,
    },
    /// a POST per message
    Webhook {
        url: String,
        format: String,
        verify: bool,
    },
    /// another cluster, over HTTP
    External {
        endpoints: Vec<String>,
        index_pattern: String,
        username: Option<String>,
        password: Option<String>,
        verify: bool,
    },
    Noop,
}

/// `'security-auditlog-'YYYY.MM.dd` (Joda letters `YYYY`, `yyyy`, `MM`,
/// `dd`, `HH`; text in quotes as written) for one instant.
pub fn index_name(pattern: &str, ts: i64) -> String {
    let (y, m, d, h, _, _) = civil(ts);
    let mut out = String::new();
    let mut quoted = false;
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            if quoted && chars.get(i + 1) == Some(&'\'') {
                out.push('\'');
                i += 2;
                continue;
            }
            quoted = !quoted;
            i += 1;
            continue;
        }
        if quoted {
            out.push(c);
            i += 1;
            continue;
        }
        let mut run = 1;
        while i + run < chars.len() && chars[i + run] == c {
            run += 1;
        }
        match c {
            'Y' | 'y' => {
                out.push_str(&if run >= 4 { format!("{y:04}") } else { format!("{:02}", y % 100) })
            }
            'M' => out.push_str(&if run >= 2 { format!("{m:02}") } else { m.to_string() }),
            'd' => out.push_str(&if run >= 2 { format!("{d:02}") } else { d.to_string() }),
            'H' => out.push_str(&if run >= 2 { format!("{h:02}") } else { h.to_string() }),
            other => {
                for _ in 0..run {
                    out.push(other);
                }
            }
        }
        i += run;
    }
    out.to_lowercase()
}

fn civil(ts: i64) -> (i64, i64, i64, i64, i64, i64) {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d, secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// `2026-09-02T19:52:09.012+00:00`
fn timestamp(millis: i64) -> String {
    let (y, m, d, h, mi, s) = civil(millis.div_euclid(1000));
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:03}+00:00", millis.rem_euclid(1000))
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---- the log ----------------------------------------------------------------------------

/// What one message needs from the request it is about.
#[derive(Clone, Debug, Default)]
pub struct RequestInfo {
    pub method: String,
    pub path: String,
    /// route parameters the path named (`index`, `id`, `name`) and the query string's
    pub params: BTreeMap<String, String>,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub remote: String,
}

/// The node's own words in every message.
#[derive(Clone, Debug)]
pub struct NodeInfo {
    pub cluster_name: String,
    pub node_name: String,
    pub node_id: String,
    pub host_address: String,
    pub host_name: String,
}

pub struct AuditLog {
    pub config: RwLock<Arc<AuditConfig>>,
    pub readonly: Vec<String>,
    pub sink: Sink,
    pub node: NodeInfo,
    tx: std::sync::mpsc::Sender<Value>,
    /// a counter per config type for `audit_compliance_doc_version`
    config_versions: parking_lot::Mutex<HashMap<String, u64>>,
    task_seq: std::sync::atomic::AtomicU64,
    /// whether any index's writes, or any field's reads, are watched at
    /// all: read on every document, so kept as a flag
    any_write_watch: std::sync::atomic::AtomicBool,
    any_read_watch: std::sync::atomic::AtomicBool,
}

impl AuditLog {
    /// Build from the node settings and `config/security/audit.yml`, and
    /// start the sink's thread. The internal sink needs the store, which
    /// is handed in later through `attach_store`.
    pub fn new(settings: &Value, enabled: bool) -> Arc<AuditLog> {
        let get = |k: &str| crate::tls::node_setting(settings, k);
        let kind =
            get("plugins.security.audit.type").unwrap_or_else(|| "internal_opensearch".into());
        let index_pattern = get("plugins.security.audit.config.index")
            .unwrap_or_else(|| "'security-auditlog-'YYYY.MM.dd".into());
        let sink = match kind.as_str() {
            "internal_opensearch" | "internal_opensearch_data_stream" => {
                Sink::Internal { index_pattern }
            }
            "debug" => Sink::Debug,
            "log4j" => Sink::Log4j {
                logger: get("plugins.security.audit.config.log4j.logger_name")
                    .unwrap_or_else(|| "audit".into()),
                level: get("plugins.security.audit.config.log4j.level")
                    .unwrap_or_else(|| "INFO".into()),
            },
            "webhook" => Sink::Webhook {
                url: get("plugins.security.audit.config.webhook.url").unwrap_or_default(),
                format: get("plugins.security.audit.config.webhook.format")
                    .unwrap_or_else(|| "JSON".into())
                    .to_uppercase(),
                verify: get("plugins.security.audit.config.webhook.ssl.verify")
                    .map(|v| v != "false")
                    .unwrap_or(true),
            },
            "external_opensearch" => Sink::External {
                endpoints: get("plugins.security.audit.config.http_endpoints")
                    .map(|v| {
                        v.split(',')
                            .map(|s| s.trim().trim_matches(['[', ']', '"', '\'']).to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_else(|| vec!["localhost:9200".into()]),
                index_pattern,
                username: get("plugins.security.audit.config.username"),
                password: get("plugins.security.audit.config.password"),
                verify: get("plugins.security.audit.config.verify_hostnames")
                    .map(|v| v != "false")
                    .unwrap_or(true),
            },
            _ => Sink::Noop,
        };
        let readonly: Vec<String> = get("plugins.security.audit.config.readonly")
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().trim_matches(['[', ']', '"', '\'']).to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let config =
            if enabled { Self::load() } else { AuditConfig::from_json(&embedded_default()) };
        let bound = crate::api::bound_address();
        let host = bound.rsplit_once(':').map(|(h, _)| h.to_string()).unwrap_or(bound.clone());
        let (tx, rx) = std::sync::mpsc::channel::<Value>();
        let log = Arc::new(AuditLog {
            config: RwLock::new(Arc::new(config)),
            readonly,
            sink: sink.clone(),
            node: NodeInfo {
                cluster_name: get("cluster.name").unwrap_or_else(|| "boostsearch".into()),
                node_name: get("node.name").unwrap_or_else(|| "boostsearch".into()),
                node_id: "node-0".into(),
                host_address: host.clone(),
                host_name: host,
            },
            tx,
            config_versions: parking_lot::Mutex::new(HashMap::new()),
            task_seq: std::sync::atomic::AtomicU64::new(1),
            any_write_watch: std::sync::atomic::AtomicBool::new(false),
            any_read_watch: std::sync::atomic::AtomicBool::new(false),
        });
        log.refresh_flags();
        if enabled {
            Self::start_sink(sink, rx);
        }
        log
    }

    fn start_sink(sink: Sink, rx: std::sync::mpsc::Receiver<Value>) {
        std::thread::Builder::new()
            .name("audit-sink".into())
            .spawn(move || {
                for msg in rx {
                    deliver(&sink, &msg);
                }
            })
            .ok();
    }

    fn file() -> std::path::PathBuf {
        super::security_dir().join("audit.yml")
    }

    /// The configuration on disk laid over the plugin's default.
    fn load() -> AuditConfig {
        let mut base = embedded_default();
        if let Ok(text) = std::fs::read_to_string(Self::file()) {
            if let Ok(y) = serde_yaml::from_str::<serde_yaml::Value>(&text) {
                if let Some(cfg) =
                    serde_json::to_value(y).ok().and_then(|v| v.get("config").cloned())
                {
                    crate::api::merge_into(&mut base, &cfg);
                }
            }
        }
        AuditConfig::from_json(&base)
    }

    fn refresh_flags(&self) {
        let cfg = self.current();
        let on = cfg.enabled && cfg.compliance.enabled;
        self.any_write_watch.store(
            on && !cfg.compliance.write_watched_indices.is_empty(),
            std::sync::atomic::Ordering::Release,
        );
        self.any_read_watch.store(
            on && !cfg.compliance.read_watched_fields.is_empty(),
            std::sync::atomic::Ordering::Release,
        );
    }

    /// Whether any write anywhere is watched; a flag, read per document.
    pub fn any_write_watched(&self) -> bool {
        self.any_write_watch.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn any_read_watched(&self) -> bool {
        self.any_read_watch.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Replace the configuration and write it out.
    pub fn store(&self, cfg: AuditConfig) {
        let doc = json!({"_meta": {"type": "audit", "config_version": 2}, "config": cfg.to_json()});
        if let Ok(text) = serde_yaml::to_string(&doc) {
            let _ = std::fs::create_dir_all(super::security_dir());
            let _ = std::fs::write(Self::file(), text);
        }
        *self.config.write() = Arc::new(cfg);
        self.refresh_flags();
    }

    pub fn current(&self) -> Arc<AuditConfig> {
        self.config.read().clone()
    }

    /// The API's view: `{"_readonly": [...], "config": {...}}`.
    pub fn api_view(&self) -> Value {
        json!({"_readonly": self.readonly, "config": self.current().to_json()})
    }

    fn base(&self, category: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("audit_cluster_name".into(), json!(self.node.cluster_name));
        m.insert("audit_node_name".into(), json!(self.node.node_name));
        m.insert("audit_category".into(), json!(category));
        m.insert("audit_request_origin".into(), json!("REST"));
        m.insert("audit_node_id".into(), json!(self.node.node_id));
        m.insert("@timestamp".into(), json!(timestamp(now_millis())));
        m.insert("audit_format_version".into(), json!(4));
        m.insert("audit_node_host_address".into(), json!(self.node.host_address));
        m.insert("audit_node_host_name".into(), json!(self.node.host_name));
        m
    }

    /// Whether any record this request could produce would quote its
    /// body, so the body need be copied at all.
    pub fn quotes_bodies(&self, admin_action: bool, security_api: bool) -> bool {
        let cfg = self.current();
        if !cfg.enabled || !cfg.audit.log_request_body {
            return false;
        }
        self.rest_allowed(&cfg, "AUTHENTICATED")
            || (security_api && self.rest_allowed(&cfg, "GRANTED_PRIVILEGES"))
            || self.transport_allowed(&cfg, "GRANTED_PRIVILEGES")
            || (admin_action && self.transport_allowed(&cfg, "INDEX_EVENT"))
    }

    fn ignored_user(&self, cfg: &AuditConfig, user: Option<&str>) -> bool {
        match user {
            Some(u) => cfg.audit.ignore_users.iter().any(|p| pattern_matches(p, u)),
            None => false,
        }
    }

    fn ignored_request(&self, cfg: &AuditConfig, what: &str) -> bool {
        cfg.audit.ignore_requests.iter().any(|p| pattern_matches(p, what))
    }

    fn rest_allowed(&self, cfg: &AuditConfig, category: &str) -> bool {
        cfg.enabled
            && cfg.audit.enable_rest
            && !cfg.audit.disabled_rest_categories.iter().any(|c| c == category)
    }

    fn transport_allowed(&self, cfg: &AuditConfig, category: &str) -> bool {
        cfg.enabled
            && cfg.audit.enable_transport
            && !cfg.audit.disabled_transport_categories.iter().any(|c| c == category)
    }

    /// The REST request's own fields.
    fn add_rest(
        &self,
        cfg: &AuditConfig,
        m: &mut Map<String, Value>,
        req: &RequestInfo,
        with_body: bool,
    ) {
        m.insert("audit_request_layer".into(), json!("REST"));
        m.insert("audit_rest_request_method".into(), json!(req.method));
        m.insert("audit_rest_request_path".into(), json!(req.path));
        if !req.remote.is_empty() {
            m.insert("audit_request_remote_address".into(), json!(req.remote));
        }
        if !req.params.is_empty() {
            let mut p = Map::new();
            for (k, v) in &req.params {
                if cfg.audit.ignore_url_params.iter().any(|x| pattern_matches(x, k)) {
                    p.insert(k.clone(), json!("REDACTED"));
                } else {
                    p.insert(k.clone(), json!(v));
                }
            }
            m.insert("audit_rest_request_params".into(), Value::Object(p));
        }
        let mut headers: Map<String, Value> = Map::new();
        for (k, v) in &req.headers {
            let lower = k.to_lowercase();
            if cfg.audit.exclude_sensitive_headers && lower == "authorization" {
                continue;
            }
            if cfg.audit.ignore_headers.iter().any(|x| pattern_matches(&x.to_lowercase(), &lower)) {
                continue;
            }
            match headers.get_mut(k) {
                Some(Value::Array(a)) => a.push(json!(v)),
                _ => {
                    headers.insert(k.clone(), json!([v]));
                }
            }
        }
        if !headers.is_empty() {
            m.insert("audit_rest_request_headers".into(), Value::Object(headers));
        }
        if with_body && cfg.audit.log_request_body {
            if let Some(b) = req.body.as_deref().filter(|b| !b.is_empty()) {
                let sensitive =
                    req.path.contains("/api/internalusers") || req.path.contains("/api/account");
                let shown = if sensitive && b.contains("password") {
                    "__SENSITIVE__".to_string()
                } else {
                    b.to_string()
                };
                m.insert("audit_request_body".into(), json!(shown));
            }
        }
    }

    fn send(&self, m: Map<String, Value>) {
        let _ = self.tx.send(Value::Object(m));
    }

    // ---- the categories ------------------------------------------------------------

    /// A login that failed: wrong password, bad token, unknown user.
    pub fn failed_login(&self, user: Option<&str>, req: &RequestInfo) {
        let cfg = self.current();
        if !self.rest_allowed(&cfg, "FAILED_LOGIN")
            || self.ignored_user(&cfg, user)
            || self.ignored_request(&cfg, &req.path)
        {
            return;
        }
        let mut m = self.base("FAILED_LOGIN");
        self.add_rest(&cfg, &mut m, req, true);
        m.insert("audit_request_effective_user_is_admin".into(), json!(false));
        if let Some(u) = user {
            m.insert("audit_request_effective_user".into(), json!(u));
        }
        self.send(m);
    }

    /// A caller told apart; off by default.
    pub fn authenticated(&self, caller: &Caller, req: &RequestInfo) {
        let cfg = self.current();
        if !self.rest_allowed(&cfg, "AUTHENTICATED")
            || self.ignored_user(&cfg, Some(&caller.name))
            || self.ignored_request(&cfg, &req.path)
        {
            return;
        }
        let mut m = self.base("AUTHENTICATED");
        m.insert("audit_request_initiating_user".into(), json!(caller.name));
        self.add_rest(&cfg, &mut m, req, true);
        m.insert("audit_request_effective_user_is_admin".into(), json!(caller.admin_cert));
        m.insert("audit_request_effective_user".into(), json!(caller.name));
        self.send(m);
    }

    /// A request carrying the plugin's own internal headers.
    pub fn bad_headers(&self, req: &RequestInfo) {
        let cfg = self.current();
        if !self.rest_allowed(&cfg, "BAD_HEADERS") {
            return;
        }
        let mut m = self.base("BAD_HEADERS");
        self.add_rest(&cfg, &mut m, req, true);
        // the plugin writes no remote address for these
        m.remove("audit_request_remote_address");
        self.send(m);
    }

    /// A transport-level action refused for a caller.
    pub fn missing_privileges(
        &self,
        caller: &Caller,
        action: &str,
        req: &RequestInfo,
        indices: &[String],
        resolved: &[String],
    ) {
        let cfg = self.current();
        if !self.transport_allowed(&cfg, "MISSING_PRIVILEGES")
            || self.ignored_user(&cfg, Some(&caller.name))
        {
            return;
        }
        let request_type = transport_request_type(action, &req.method, &req.path);
        if self.ignored_request(&cfg, &request_type) {
            return;
        }
        let mut m = self.base("MISSING_PRIVILEGES");
        self.add_transport(&cfg, &mut m, caller, action, &request_type, req, indices, resolved);
        self.send(m);
    }

    /// A transport-level action allowed; off by default.
    pub fn granted_privileges(
        &self,
        caller: &Caller,
        action: &str,
        req: &RequestInfo,
        indices: &[String],
        resolved: &[String],
    ) {
        let cfg = self.current();
        if !self.transport_allowed(&cfg, "GRANTED_PRIVILEGES")
            || self.ignored_user(&cfg, Some(&caller.name))
        {
            return;
        }
        let request_type = transport_request_type(action, &req.method, &req.path);
        if self.ignored_request(&cfg, &request_type) {
            return;
        }
        let mut m = self.base("GRANTED_PRIVILEGES");
        if action == "indices:data/write/bulk" {
            // a bulk is judged shard by shard later; the request itself names no index
            self.add_transport(&cfg, &mut m, caller, action, &request_type, req, &[], &[]);
        } else if action.starts_with("indices:admin/") {
            self.add_transport(&cfg, &mut m, caller, action, &request_type, req, indices, &[]);
        } else {
            self.add_transport(&cfg, &mut m, caller, action, &request_type, req, indices, resolved);
        }
        self.send(m);
    }

    /// The security REST API allowed for a caller: a REST-layer grant.
    pub fn granted_rest(&self, caller: &Caller, req: &RequestInfo) {
        let cfg = self.current();
        if !self.rest_allowed(&cfg, "GRANTED_PRIVILEGES")
            || self.ignored_user(&cfg, Some(&caller.name))
            || self.ignored_request(&cfg, &req.path)
        {
            return;
        }
        let mut m = self.base("GRANTED_PRIVILEGES");
        self.add_rest(&cfg, &mut m, req, true);
        m.insert("audit_request_effective_user".into(), json!(caller.name));
        self.send(m);
    }

    /// An index-administration action carried out.
    pub fn index_event(
        &self,
        caller: &Caller,
        action: &str,
        req: &RequestInfo,
        indices: &[String],
        resolved: &[String],
        body: Option<&str>,
    ) {
        let cfg = self.current();
        if !self.transport_allowed(&cfg, "INDEX_EVENT")
            || self.ignored_user(&cfg, Some(&caller.name))
        {
            return;
        }
        let request_type = transport_request_type(action, &req.method, &req.path);
        if self.ignored_request(&cfg, &request_type) {
            return;
        }
        let mut m = self.base("INDEX_EVENT");
        let _ = resolved;
        // an index the request names is reported as named, not as resolved
        self.add_transport(&cfg, &mut m, caller, action, &request_type, req, indices, &[]);
        if let Some(b) = body.filter(|b| !b.is_empty()) {
            if cfg.audit.log_request_body {
                m.insert("audit_request_body".into(), json!(b));
            }
        }
        self.send(m);
    }

    /// An index event raised from inside a write (auto-create, auto-put):
    /// the plugin writes both a grant and an index event for it.
    pub fn index_event_inner(
        &self,
        caller: &Caller,
        action: &str,
        index: &str,
        body: &str,
        with_headers: bool,
    ) {
        let cfg = self.current();
        if self.ignored_user(&cfg, Some(&caller.name)) {
            return;
        }
        let request_type = transport_request_type(action, "", "");
        if self.ignored_request(&cfg, &request_type) {
            return;
        }
        let req = RequestInfo { remote: caller.remote_address.clone(), ..Default::default() };
        let indices = vec![index.to_string()];
        for category in ["GRANTED_PRIVILEGES", "INDEX_EVENT"] {
            if !self.transport_allowed(&cfg, category) {
                continue;
            }
            let mut m = self.base(category);
            if with_headers {
                m.insert(
                    "audit_transport_headers".into(),
                    json!({
                        "_opendistro_security_remotecn": self.node.cluster_name,
                        "_opendistro_security_initial_action_class_header": "BulkShardRequest",
                        "_opendistro_security_origin_header": "REST",
                    }),
                );
            }
            if action == "indices:admin/auto_create" {
                self.add_transport(
                    &cfg,
                    &mut m,
                    caller,
                    action,
                    &request_type,
                    &req,
                    &indices,
                    &[],
                );
            } else {
                self.add_transport(
                    &cfg,
                    &mut m,
                    caller,
                    action,
                    &request_type,
                    &req,
                    &[],
                    &indices,
                );
            }
            if cfg.audit.log_request_body {
                m.insert("audit_request_body".into(), json!(body));
            }
            self.send(m);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_transport(
        &self,
        cfg: &AuditConfig,
        m: &mut Map<String, Value>,
        caller: &Caller,
        action: &str,
        request_type: &str,
        req: &RequestInfo,
        indices: &[String],
        resolved: &[String],
    ) {
        let seq = self.task_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        m.insert("audit_trace_task_id".into(), json!(format!("{}:{seq}", self.node.node_id)));
        m.insert("audit_transport_request_type".into(), json!(request_type));
        m.insert("audit_request_layer".into(), json!("TRANSPORT"));
        if !req.remote.is_empty() {
            m.insert("audit_request_remote_address".into(), json!(req.remote));
        }
        m.insert("audit_request_privilege".into(), json!(action));
        m.insert("audit_request_effective_user".into(), json!(caller.name));
        if !indices.is_empty() {
            m.insert("audit_trace_indices".into(), json!(indices));
        }
        if cfg.audit.resolve_indices && !resolved.is_empty() {
            m.insert("audit_trace_resolved_indices".into(), json!(resolved));
        }
        if let Some(id) = req.params.get("id") {
            if action.starts_with("indices:data/") {
                m.insert("audit_trace_doc_id".into(), json!(id));
            }
        }
        if cfg.audit.log_request_body {
            if action.starts_with("indices:data/write/index") {
                if let Some(b) = req.body.as_deref().filter(|b| !b.is_empty()) {
                    m.insert("audit_request_body".into(), json!(b));
                }
            } else if action == "indices:data/read/search" || action == "indices:admin/auto_create"
            {
                // a search's body is quoted even when there was none
                let b = req.body.as_deref().filter(|b| !b.trim().is_empty()).unwrap_or("{}");
                m.insert("audit_request_body".into(), json!(b));
            }
        }
    }

    // ---- compliance ----------------------------------------------------------------

    fn compliance_on(&self, cfg: &AuditConfig) -> bool {
        cfg.enabled && cfg.compliance.enabled
    }

    /// Whether writes to an index are watched for a user.
    pub fn watches_write(&self, index: &str, user: &str) -> bool {
        let cfg = self.current();
        self.compliance_on(&cfg)
            && cfg.compliance.write_watched_indices.iter().any(|p| pattern_matches(p, index))
            && !cfg.compliance.write_ignore_users.iter().any(|p| pattern_matches(p, user))
    }

    /// The fields of an index whose reads are watched for a user.
    pub fn watched_read_fields(&self, index: &str, user: &str) -> Vec<String> {
        let cfg = self.current();
        if !self.compliance_on(&cfg)
            || cfg.compliance.read_ignore_users.iter().any(|p| pattern_matches(p, user))
        {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (pat, fields) in &cfg.compliance.read_watched_fields {
            if pattern_matches(pat, index) {
                out.extend(fields.iter().cloned());
            }
        }
        out
    }

    /// A document written in a watched index.
    #[allow(clippy::too_many_arguments)]
    pub fn document_written(
        &self,
        caller: &Caller,
        remote: &str,
        index: &str,
        id: &str,
        version: u64,
        before: Option<&Value>,
        after: Option<&Value>,
        deleted: bool,
    ) {
        let cfg = self.current();
        if !self.watches_write(index, &caller.name) {
            return;
        }
        let mut m = self.base("COMPLIANCE_DOC_WRITE");
        m.remove("audit_request_layer");
        let op = if deleted {
            "DELETE"
        } else if before.is_none() {
            "CREATE"
        } else {
            "UPDATE"
        };
        m.insert("audit_compliance_operation".into(), json!(op));
        m.insert("audit_compliance_doc_version".into(), json!(version));
        if !remote.is_empty() {
            m.insert("audit_request_remote_address".into(), json!(remote));
        }
        m.insert("audit_trace_doc_id".into(), json!(id));
        if !deleted {
            if cfg.compliance.write_log_diffs {
                let diff = json_diff(
                    before.unwrap_or(&Value::Object(Map::new())),
                    after.unwrap_or(&Value::Object(Map::new())),
                );
                m.insert("audit_compliance_diff_is_noop".into(), json!(diff.is_empty()));
                m.insert(
                    "audit_compliance_diff_content".into(),
                    json!(if diff.is_empty() {
                        String::new()
                    } else {
                        Value::Array(diff).to_string()
                    }),
                );
            } else if !cfg.compliance.write_metadata_only {
                if let Some(a) = after {
                    m.insert("audit_compliance_stored_fields_content".into(), json!(a.to_string()));
                }
            }
        }
        m.insert("audit_request_effective_user".into(), json!(caller.name));
        m.insert("audit_trace_shard_id".into(), json!(0));
        m.insert("audit_trace_indices".into(), json!([index]));
        m.insert("audit_trace_resolved_indices".into(), json!([index]));
        self.send(m);
    }

    /// A document with watched fields read.
    pub fn document_read(&self, user: &str, index: &str, id: &str, source: &Value) {
        let fields = self.watched_read_fields(index, user);
        if fields.is_empty() {
            return;
        }
        let cfg = self.current();
        let mut found = Map::new();
        for f in &fields {
            if let Some(v) = f.split('.').try_fold(source, |cur, p| cur.get(p)) {
                found.insert(f.clone(), v.clone());
            } else if let Some(v) = source.get(f) {
                found.insert(f.clone(), v.clone());
            }
        }
        if found.is_empty() {
            return;
        }
        let mut m = self.base("COMPLIANCE_DOC_READ");
        m.remove("audit_request_layer");
        if !cfg.compliance.read_metadata_only {
            m.insert("audit_request_body".into(), json!(Value::Object(found).to_string()));
        }
        m.insert("audit_trace_doc_id".into(), json!(id));
        m.insert("audit_request_effective_user".into(), json!(user));
        m.insert("audit_trace_shard_id".into(), json!(0));
        m.insert("audit_trace_indices".into(), json!([index]));
        m.insert("audit_trace_resolved_indices".into(), json!([index]));
        self.send(m);
    }

    /// The security configuration read through the API.
    pub fn internal_config_read(&self, caller: &Caller, remote: &str, kind: &str) {
        self.internal_config_read_with(caller, remote, kind, None)
    }

    /// The configuration read: for one resource the plugin quotes the
    /// entry, for a whole kind the field names.
    pub fn internal_config_read_with(
        &self,
        caller: &Caller,
        remote: &str,
        kind: &str,
        entry: Option<&Value>,
    ) {
        let cfg = self.current();
        if !self.compliance_on(&cfg) || !cfg.compliance.internal_config {
            return;
        }
        let mut m = self.base("COMPLIANCE_INTERNAL_CONFIG_READ");
        if cfg.compliance.read_metadata_only {
            m.insert(
                "audit_request_body".into(),
                json!(json!({"field_names": [kind]}).to_string()),
            );
        } else {
            // the whole document of that kind, hashes hidden, and no caller
            let doc = entry.cloned().unwrap_or(Value::Null);
            m.insert("audit_request_body".into(), json!(hide_hashes(&doc).to_string()));
            m.remove("audit_request_origin");
            m.insert("audit_trace_shard_id".into(), json!(0));
        }
        if !remote.is_empty() {
            m.insert("audit_request_remote_address".into(), json!(remote));
        }
        m.insert("audit_trace_doc_id".into(), json!(kind));
        if cfg.compliance.read_metadata_only {
            m.insert("audit_request_effective_user".into(), json!(caller.name));
        }
        m.insert("audit_trace_indices".into(), json!([".opendistro_security"]));
        m.insert("audit_trace_resolved_indices".into(), json!([".opendistro_security"]));
        self.send(m);
    }

    /// The security configuration changed through the API.
    pub fn internal_config_written(&self, caller: &Caller, remote: &str, kind: &str) {
        self.internal_config_written_with(caller, remote, kind, None, None)
    }

    /// The configuration changed, with the documents before and after so
    /// the diff can be written when `write_log_diffs` asks for it.
    pub fn internal_config_written_with(
        &self,
        caller: &Caller,
        remote: &str,
        kind: &str,
        before: Option<&Value>,
        after: Option<&Value>,
    ) {
        let cfg = self.current();
        if !self.compliance_on(&cfg) || !cfg.compliance.internal_config {
            return;
        }
        let version = {
            let mut v = self.config_versions.lock();
            let e = v.entry(kind.to_string()).or_insert(1);
            *e += 1;
            *e
        };
        let mut m = self.base("COMPLIANCE_INTERNAL_CONFIG_WRITE");
        m.insert("audit_compliance_operation".into(), json!("UPDATE"));
        m.insert("audit_compliance_doc_version".into(), json!(version));
        if !remote.is_empty() {
            m.insert("audit_request_remote_address".into(), json!(remote));
        }
        if cfg.compliance.write_log_diffs {
            if let (Some(b), Some(a)) = (before, after) {
                let diff = json_diff(&hide_hashes(b), &hide_hashes(a));
                m.insert("audit_compliance_diff_is_noop".into(), json!(diff.is_empty()));
                m.insert(
                    "audit_compliance_diff_content".into(),
                    json!(if diff.is_empty() {
                        String::new()
                    } else {
                        Value::Array(diff).to_string()
                    }),
                );
            }
        }
        m.insert("audit_trace_doc_id".into(), json!(kind));
        m.insert("audit_request_effective_user".into(), json!(caller.name));
        m.insert("audit_trace_shard_id".into(), json!(0));
        m.insert("audit_trace_indices".into(), json!([".opendistro_security"]));
        m.insert("audit_trace_resolved_indices".into(), json!([".opendistro_security"]));
        self.send(m);
    }
}

/// A configuration document with every `hash` shown as `__HASH__`.
fn hide_hashes(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| {
                    if k == "hash" {
                        (k.clone(), json!("__HASH__"))
                    } else {
                        (k.clone(), hide_hashes(x))
                    }
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(hide_hashes).collect()),
        other => other.clone(),
    }
}

/// The Java request class a REST call becomes, as the plugin names it in
/// `audit_transport_request_type`.
pub fn transport_request_type(action: &str, method: &str, path: &str) -> String {
    let tail = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let name = match action {
        "cluster:monitor/health" => "ClusterHealthRequest",
        "cluster:monitor/state" => "ClusterStateRequest",
        "cluster:monitor/stats" => "ClusterStatsRequest",
        "cluster:monitor/nodes/info" => "NodesInfoRequest",
        "cluster:monitor/nodes/stats" => "NodesStatsRequest",
        "cluster:monitor/main" => "MainRequest",
        "cluster:monitor/task" => "ListTasksRequest",
        "cluster:admin/settings/get" => "ClusterGetSettingsRequest",
        "cluster:admin/settings/update" => "ClusterUpdateSettingsRequest",
        "indices:data/read/search" => {
            if tail == "_count" {
                "SearchRequest"
            } else {
                "SearchRequest"
            }
        }
        "indices:data/read/msearch" => "MultiSearchRequest",
        "indices:data/read/scroll" => "SearchScrollRequest",
        "indices:data/read/get" => "GetRequest",
        "indices:data/read/mget" => "MultiGetRequest",
        "indices:data/read/explain" => "ExplainRequest",
        "indices:data/read/field_caps" => "FieldCapabilitiesRequest",
        "indices:data/read/tv" => "TermVectorsRequest",
        "indices:data/read/mtv" => "MultiTermVectorsRequest",
        "indices:data/write/index" => "IndexRequest",
        "indices:data/write/delete" => "DeleteRequest",
        "indices:data/write/update" => "UpdateRequest",
        "indices:data/write/bulk" => "BulkRequest",
        "indices:data/write/bulk[s]" => "BulkShardRequest",
        "indices:data/write/delete/byquery" => "DeleteByQueryRequest",
        "indices:data/write/update/byquery" => "UpdateByQueryRequest",
        "indices:data/write/reindex" => "ReindexRequest",
        "indices:admin/create" | "indices:admin/auto_create" => "CreateIndexRequest",
        "indices:admin/delete" => "DeleteIndexRequest",
        "indices:admin/get" => "GetIndexRequest",
        "indices:admin/mapping/put" | "indices:admin/mapping/auto_put" => "PutMappingRequest",
        "indices:admin/mappings/get" => "GetMappingsRequest",
        "indices:admin/settings/update" => "UpdateSettingsRequest",
        "indices:monitor/settings/get" => "GetSettingsRequest",
        "indices:admin/aliases" => "IndicesAliasesRequest",
        "indices:admin/aliases/get" => "GetAliasesRequest",
        "indices:admin/refresh" => "RefreshRequest",
        "indices:admin/flush" => "FlushRequest",
        "indices:admin/forcemerge" => "ForceMergeRequest",
        "indices:admin/cache/clear" => "ClearIndicesCacheRequest",
        "indices:admin/open" => "OpenIndexRequest",
        "indices:admin/close" => "CloseIndexRequest",
        "indices:monitor/stats" => "IndicesStatsRequest",
        "indices:monitor/segments" => "IndicesSegmentsRequest",
        "indices:admin/analyze" => "AnalyzeAction$Request",
        "indices:admin/validate/query" => "ValidateQueryRequest",
        "indices:admin/rollover" => "RolloverRequest",
        "indices:admin/resize" => "ResizeRequest",
        "indices:admin/template/get" => "GetIndexTemplatesRequest",
        "indices:admin/template/put" => "PutIndexTemplateRequest",
        "indices:admin/template/delete" => "DeleteIndexTemplateRequest",
        "indices:admin/index_template/get" => "GetComposableIndexTemplateAction$Request",
        "indices:admin/index_template/put" => "PutComposableIndexTemplateAction$Request",
        "indices:admin/index_template/delete" => "DeleteComposableIndexTemplateAction$Request",
        "cluster:admin/ingest/pipeline/put" => "PutPipelineRequest",
        "cluster:admin/ingest/pipeline/get" => "GetPipelineRequest",
        "cluster:admin/ingest/pipeline/delete" => "DeletePipelineRequest",
        "cluster:admin/ingest/pipeline/simulate" => "SimulatePipelineRequest",
        "cluster:admin/script/put" => "PutStoredScriptRequest",
        "cluster:admin/script/get" => "GetStoredScriptRequest",
        "cluster:admin/script/delete" => "DeleteStoredScriptRequest",
        "indices:data/read/point_in_time/create" => "CreatePitRequest",
        "indices:data/read/point_in_time/delete" => "DeletePitRequest",
        _ => "",
    };
    if !name.is_empty() {
        return name.to_string();
    }
    let _ = method;
    // an action this table does not name: its last word, as a request class
    let last = action.rsplit('/').next().unwrap_or(action);
    let mut s = String::new();
    for part in last.split(['_', '-']) {
        let mut c = part.chars();
        if let Some(f) = c.next() {
            s.push(f.to_ascii_uppercase());
            s.push_str(c.as_str());
        }
    }
    format!("{s}Request")
}

fn initial_action_class(method: &str, path: &str) -> String {
    if path.contains("/_bulk") {
        return "BulkShardRequest".into();
    }
    if path.contains("/_doc") || path.contains("/_create") {
        return if method == "DELETE" { "DeleteRequest".into() } else { "IndexRequest".into() };
    }
    if path.contains("/_update") {
        return "UpdateRequest".into();
    }
    "IndexRequest".into()
}

/// A JSON patch from one document to the next: `add`, `replace`,
/// `remove`, with JSON-pointer paths, walking objects; arrays and scalars
/// are replaced whole.
pub fn json_diff(before: &Value, after: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    diff_into(before, after, "", &mut out);
    out
}

fn escape_pointer(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

fn diff_into(before: &Value, after: &Value, path: &str, out: &mut Vec<Value>) {
    match (before, after) {
        (Value::Object(b), Value::Object(a)) => {
            for (k, bv) in b {
                let p = format!("{path}/{}", escape_pointer(k));
                match a.get(k) {
                    Some(av) => diff_into(bv, av, &p, out),
                    None => out.push(json!({"op": "remove", "path": p})),
                }
            }
            for (k, av) in a {
                if !b.contains_key(k) {
                    out.push(json!({"op": "add", "path": format!("{path}/{}", escape_pointer(k)), "value": av}));
                }
            }
        }
        (b, a) => {
            if b != a {
                out.push(json!({"op": "replace", "path": if path.is_empty() { "/".to_string() } else { path.to_string() }, "value": a}));
            }
        }
    }
}

// ---- delivery -----------------------------------------------------------------------------

static STORE: std::sync::OnceLock<crate::store::Store> = std::sync::OnceLock::new();

/// The internal sink writes through the store, which exists after the
/// log does; it is handed over once, at startup.
pub fn attach_store(store: &crate::store::Store) {
    let _ = STORE.set(store.clone());
}

pub(crate) fn attached_store() -> Option<&'static crate::store::Store> {
    STORE.get()
}

fn deliver(sink: &Sink, msg: &Value) {
    match sink {
        Sink::Noop => {}
        Sink::Debug => eprintln!("AUDIT {}", msg),
        Sink::Log4j { logger, level } => eprintln!("[{level}][{logger}] {}", msg),
        Sink::Internal { index_pattern } => {
            let Some(store) = STORE.get() else { return };
            let ts = now_millis() / 1000;
            let index = index_name(index_pattern, ts);
            let _ = crate::api::index_audit_document(store, &index, msg);
        }
        Sink::Webhook { url, format, verify } => {
            if url.is_empty() {
                return;
            }
            let agent: ureq::Agent = if *verify {
                ureq::Agent::config_builder().build().into()
            } else {
                ureq::Agent::config_builder()
                    .tls_config(ureq::tls::TlsConfig::builder().disable_verification(true).build())
                    .build()
                    .into()
            };
            let _ = match format.as_str() {
                "TEXT" => agent
                    .post(url)
                    .header("Content-Type", "text/plain")
                    .send(msg.to_string().as_bytes())
                    .map(|_| ()),
                "SLACK" => agent
                    .post(url)
                    .header("Content-Type", "application/json")
                    .send(json!({"text": msg.to_string()}).to_string().as_bytes())
                    .map(|_| ()),
                "URL_PARAMETER_GET" => agent
                    .get(&format!(
                        "{url}{}",
                        percent_encoding::utf8_percent_encode(
                            &msg.to_string(),
                            percent_encoding::NON_ALPHANUMERIC
                        )
                    ))
                    .call()
                    .map(|_| ()),
                "URL_PARAMETER_POST" => agent
                    .post(&format!(
                        "{url}{}",
                        percent_encoding::utf8_percent_encode(
                            &msg.to_string(),
                            percent_encoding::NON_ALPHANUMERIC
                        )
                    ))
                    .send(&[] as &[u8])
                    .map(|_| ()),
                _ => agent
                    .post(url)
                    .header("Content-Type", "application/json")
                    .send(msg.to_string().as_bytes())
                    .map(|_| ()),
            };
        }
        Sink::External { endpoints, index_pattern, username, password, verify } => {
            let ts = now_millis() / 1000;
            let index = index_name(index_pattern, ts);
            let agent: ureq::Agent = if *verify {
                ureq::Agent::config_builder().build().into()
            } else {
                ureq::Agent::config_builder()
                    .tls_config(ureq::tls::TlsConfig::builder().disable_verification(true).build())
                    .build()
                    .into()
            };
            for ep in endpoints {
                let base = if ep.contains("://") { ep.clone() } else { format!("http://{ep}") };
                let mut req = agent
                    .post(&format!("{base}/{index}/_doc"))
                    .header("Content-Type", "application/json");
                if let (Some(u), Some(p)) = (username, password) {
                    use base64::Engine;
                    let token =
                        base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"));
                    req = req.header("Authorization", &format!("Basic {token}"));
                }
                if req.send(msg.to_string().as_bytes()).is_ok() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_index_by_day() {
        assert_eq!(
            index_name("'security-auditlog-'YYYY.MM.dd", 1_788_375_872),
            "security-auditlog-2026.09.02"
        );
        assert_eq!(index_name("'audit-'yyyy-MM-dd-HH", 1_788_375_872), "audit-2026-09-02-19");
    }

    #[test]
    fn writes_timestamps_like_joda() {
        assert_eq!(timestamp(1_788_375_872_012), "2026-09-02T19:04:32.012+00:00");
    }

    #[test]
    fn diffs_as_json_patch() {
        let d = json_diff(&json!({"secret": "s1", "n": 1}), &json!({"secret": "s2", "n": 2}));
        assert_eq!(
            d,
            vec![
                json!({"op": "replace", "path": "/secret", "value": "s2"}),
                json!({"op": "replace", "path": "/n", "value": 2})
            ]
        );
        let d = json_diff(&json!({}), &json!({"secret": "s1", "n": 1}));
        assert_eq!(d.len(), 2);
        assert_eq!(d[0]["op"], "add");
    }
}
