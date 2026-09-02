//! Who is asking, and what they may do.
//!
//! This carries what OpenSearch's security plugin carries: internal users
//! with bcrypt hashes, roles with cluster and index permissions, role
//! mappings from users and backend roles to roles, action groups that name
//! sets of permissions, and tenants. The caller's identity is worked out
//! once per request (`layer.rs`), and the evaluator here says whether an
//! action on some indices is allowed -- the way the plugin's
//! `PrivilegesEvaluator` says it, so the same roles give the same answers.
//!
//! Security is off until `plugins.security.disabled: false` is set; while
//! off, every caller is the admin and nothing here is consulted.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::{Map, Value, json};

pub mod api;
pub mod authc;
pub mod layer;
pub mod saml;
pub mod view;

/// One internal user, as `internal_users.yml` writes it.
#[derive(Clone, Debug, Default)]
pub struct InternalUser {
    pub hash: String,
    pub reserved: bool,
    pub hidden: bool,
    pub backend_roles: Vec<String>,
    /// roles given to the user outright, beside what the mappings give
    pub security_roles: Vec<String>,
    pub attributes: BTreeMap<String, String>,
    pub description: Option<String>,
    pub is_static: bool,
}

/// One index permission block of a role.
#[derive(Clone, Debug, Default)]
pub struct IndexPermission {
    pub index_patterns: Vec<String>,
    pub allowed_actions: Vec<String>,
    /// document-level security: a query the caller's view is filtered by
    pub dls: Option<String>,
    /// field-level security: `field`, `~field` (excluded), wildcards
    pub fls: Vec<String>,
    pub masked_fields: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct TenantPermission {
    pub tenant_patterns: Vec<String>,
    pub allowed_actions: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Role {
    pub reserved: bool,
    pub hidden: bool,
    pub is_static: bool,
    pub description: Option<String>,
    pub cluster_permissions: Vec<String>,
    pub index_permissions: Vec<IndexPermission>,
    pub tenant_permissions: Vec<TenantPermission>,
}

#[derive(Clone, Debug, Default)]
pub struct RoleMapping {
    pub reserved: bool,
    pub hidden: bool,
    pub users: Vec<String>,
    pub backend_roles: Vec<String>,
    pub and_backend_roles: Vec<String>,
    pub hosts: Vec<String>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ActionGroup {
    pub reserved: bool,
    pub hidden: bool,
    pub is_static: bool,
    pub kind: Option<String>,
    pub description: Option<String>,
    pub allowed_actions: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Tenant {
    pub reserved: bool,
    pub hidden: bool,
    pub is_static: bool,
    pub description: Option<String>,
}

/// The whole security configuration, as one snapshot.
#[derive(Clone, Debug, Default)]
pub struct SecurityConfig {
    pub users: BTreeMap<String, InternalUser>,
    pub roles: BTreeMap<String, Role>,
    pub mappings: BTreeMap<String, RoleMapping>,
    pub action_groups: BTreeMap<String, ActionGroup>,
    pub tenants: BTreeMap<String, Tenant>,
    /// `config.yml`'s dynamic section, kept as JSON
    pub dynamic: Value,
    /// action groups flattened into the action patterns they stand for
    flat_groups: HashMap<String, HashSet<String>>,
}

// ---- reading the YAML shapes ------------------------------------------------

fn yaml_to_json(text: &str) -> Value {
    serde_yaml::from_str::<serde_yaml::Value>(text)
        .ok()
        .and_then(|y| serde_json::to_value(y).ok())
        .unwrap_or(Value::Object(Map::new()))
}

fn strings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| match x {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn flag(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => false,
    }
}

fn text(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(other) => Some(other.to_string()),
    }
}

impl InternalUser {
    pub fn from_json(v: &Value) -> InternalUser {
        InternalUser {
            hash: text(v.get("hash")).unwrap_or_default(),
            reserved: flag(v.get("reserved")),
            hidden: flag(v.get("hidden")),
            backend_roles: strings(v.get("backend_roles")),
            security_roles: strings(v.get("opendistro_security_roles")),
            attributes: v
                .get("attributes")
                .and_then(|a| a.as_object())
                .map(|o| {
                    o.iter().map(|(k, x)| (k.clone(), text(Some(x)).unwrap_or_default())).collect()
                })
                .unwrap_or_default(),
            description: text(v.get("description")),
            is_static: flag(v.get("static")),
        }
    }

    /// The user as the API reports it: never the hash itself.
    pub fn to_json(&self) -> Value {
        let mut o = json!({
            "hash": "",
            "reserved": self.reserved,
            "hidden": self.hidden,
            "backend_roles": self.backend_roles,
            "attributes": self.attributes,
        });
        if let Some(d) = &self.description {
            o["description"] = json!(d);
        }
        o["opendistro_security_roles"] = json!(self.security_roles);
        o["static"] = json!(self.is_static);
        o
    }
}

impl IndexPermission {
    fn from_json(v: &Value) -> IndexPermission {
        IndexPermission {
            index_patterns: strings(v.get("index_patterns")),
            allowed_actions: strings(v.get("allowed_actions")),
            dls: text(v.get("dls")).filter(|s| !s.trim().is_empty()),
            fls: strings(v.get("fls")),
            masked_fields: strings(v.get("masked_fields")),
        }
    }

    fn to_json(&self) -> Value {
        let mut o = json!({
            "index_patterns": self.index_patterns,
            "fls": self.fls,
            "masked_fields": self.masked_fields,
            "allowed_actions": self.allowed_actions,
        });
        if let Some(d) = &self.dls {
            o["dls"] = json!(d);
        }
        o
    }
}

impl Role {
    pub fn from_json(v: &Value) -> Role {
        Role {
            reserved: flag(v.get("reserved")),
            hidden: flag(v.get("hidden")),
            is_static: flag(v.get("static")),
            description: text(v.get("description")),
            cluster_permissions: strings(v.get("cluster_permissions")),
            index_permissions: v
                .get("index_permissions")
                .and_then(|a| a.as_array())
                .map(|a| a.iter().map(IndexPermission::from_json).collect())
                .unwrap_or_default(),
            tenant_permissions: v
                .get("tenant_permissions")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .map(|t| TenantPermission {
                            tenant_patterns: strings(t.get("tenant_patterns")),
                            allowed_actions: strings(t.get("allowed_actions")),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    pub fn to_json(&self) -> Value {
        let mut o = json!({
            "reserved": self.reserved,
            "hidden": self.hidden,
        });
        if let Some(d) = &self.description {
            o["description"] = json!(d);
        }
        o["cluster_permissions"] = json!(self.cluster_permissions);
        o["index_permissions"] =
            Value::Array(self.index_permissions.iter().map(|p| p.to_json()).collect());
        o["tenant_permissions"] = Value::Array(
            self.tenant_permissions
                .iter()
                .map(|t| json!({"tenant_patterns": t.tenant_patterns, "allowed_actions": t.allowed_actions}))
                .collect(),
        );
        o["static"] = json!(self.is_static);
        o
    }
}

impl RoleMapping {
    pub fn from_json(v: &Value) -> RoleMapping {
        RoleMapping {
            reserved: flag(v.get("reserved")),
            hidden: flag(v.get("hidden")),
            users: strings(v.get("users")),
            backend_roles: strings(v.get("backend_roles")),
            and_backend_roles: strings(v.get("and_backend_roles")),
            hosts: strings(v.get("hosts")),
            description: text(v.get("description")),
        }
    }

    pub fn to_json(&self) -> Value {
        let mut o = json!({
            "hosts": self.hosts,
            "users": self.users,
            "reserved": self.reserved,
            "hidden": self.hidden,
            "backend_roles": self.backend_roles,
            "and_backend_roles": self.and_backend_roles,
        });
        if let Some(d) = &self.description {
            o["description"] = json!(d);
        }
        o
    }
}

impl ActionGroup {
    pub fn from_json(v: &Value) -> ActionGroup {
        ActionGroup {
            reserved: flag(v.get("reserved")),
            hidden: flag(v.get("hidden")),
            is_static: flag(v.get("static")),
            kind: text(v.get("type")),
            description: text(v.get("description")),
            allowed_actions: strings(v.get("allowed_actions")),
        }
    }

    pub fn to_json(&self) -> Value {
        let mut o = json!({
            "reserved": self.reserved,
            "hidden": self.hidden,
            "allowed_actions": self.allowed_actions,
        });
        if let Some(k) = &self.kind {
            o["type"] = json!(k);
        }
        if let Some(d) = &self.description {
            o["description"] = json!(d);
        }
        o["static"] = json!(self.is_static);
        o
    }
}

impl Tenant {
    pub fn from_json(v: &Value) -> Tenant {
        Tenant {
            reserved: flag(v.get("reserved")),
            hidden: flag(v.get("hidden")),
            is_static: flag(v.get("static")),
            description: text(v.get("description")),
        }
    }

    pub fn to_json(&self) -> Value {
        let mut o = json!({"reserved": self.reserved, "hidden": self.hidden});
        if let Some(d) = &self.description {
            o["description"] = json!(d);
        }
        o["static"] = json!(self.is_static);
        o
    }
}

/// The entries of one config document, less its `_meta`.
fn entries(doc: &Value) -> Vec<(String, Value)> {
    doc.as_object()
        .map(|o| {
            o.iter().filter(|(k, _)| *k != "_meta").map(|(k, v)| (k.clone(), v.clone())).collect()
        })
        .unwrap_or_default()
}

impl SecurityConfig {
    /// The configuration the plugin ships with: its static roles, action
    /// groups and tenants, and the demo users and mappings.
    pub fn defaults() -> SecurityConfig {
        let mut c = SecurityConfig::default();
        for (name, v) in entries(&yaml_to_json(include_str!("defaults/static_action_groups.yml"))) {
            let mut g = ActionGroup::from_json(&v);
            g.is_static = true;
            g.reserved = true;
            c.action_groups.insert(name, g);
        }
        for (name, v) in entries(&yaml_to_json(include_str!("defaults/static_roles.yml"))) {
            let mut r = Role::from_json(&v);
            r.is_static = true;
            c.roles.insert(name, r);
        }
        for (name, v) in entries(&yaml_to_json(include_str!("defaults/static_tenants.yml"))) {
            let mut t = Tenant::from_json(&v);
            t.is_static = true;
            c.tenants.insert(name, t);
        }
        c.merge_documents(&[
            ("internalusers", yaml_to_json(include_str!("defaults/internal_users.yml"))),
            ("roles", yaml_to_json(include_str!("defaults/roles.yml"))),
            ("rolesmapping", yaml_to_json(include_str!("defaults/roles_mapping.yml"))),
            ("actiongroups", yaml_to_json(include_str!("defaults/action_groups.yml"))),
            ("tenants", yaml_to_json(include_str!("defaults/tenants.yml"))),
            ("config", yaml_to_json(include_str!("defaults/config.yml"))),
        ]);
        c
    }

    /// Lay documents of each kind over what is there.
    pub fn merge_documents(&mut self, docs: &[(&str, Value)]) {
        for (kind, doc) in docs {
            match *kind {
                "internalusers" => {
                    for (name, v) in entries(doc) {
                        self.users.insert(name, InternalUser::from_json(&v));
                    }
                }
                "roles" => {
                    for (name, v) in entries(doc) {
                        self.roles.insert(name, Role::from_json(&v));
                    }
                }
                "rolesmapping" => {
                    for (name, v) in entries(doc) {
                        self.mappings.insert(name, RoleMapping::from_json(&v));
                    }
                }
                "actiongroups" => {
                    for (name, v) in entries(doc) {
                        self.action_groups.insert(name, ActionGroup::from_json(&v));
                    }
                }
                "tenants" => {
                    for (name, v) in entries(doc) {
                        self.tenants.insert(name, Tenant::from_json(&v));
                    }
                }
                "config" => {
                    self.dynamic = doc.get("config").cloned().unwrap_or(Value::Object(Map::new()));
                }
                _ => {}
            }
        }
        self.flatten_groups();
    }

    /// Every action group as the set of action patterns it stands for,
    /// groups inside groups followed to the actions at the bottom.
    fn flatten_groups(&mut self) {
        let mut flat: HashMap<String, HashSet<String>> = HashMap::new();
        for name in self.action_groups.keys() {
            let mut out = HashSet::new();
            let mut seen = HashSet::new();
            self.expand_into(name, &mut out, &mut seen);
            flat.insert(name.clone(), out);
        }
        self.flat_groups = flat;
    }

    fn expand_into(&self, name: &str, out: &mut HashSet<String>, seen: &mut HashSet<String>) {
        if !seen.insert(name.to_string()) {
            return;
        }
        let Some(g) = self.action_groups.get(name) else {
            out.insert(name.to_string());
            return;
        };
        for a in &g.allowed_actions {
            if self.action_groups.contains_key(a) {
                self.expand_into(a, out, seen);
            } else {
                out.insert(a.clone());
            }
        }
    }

    /// The action patterns a list of permissions (actions and groups) names.
    pub fn resolve_actions(&self, perms: &[String]) -> HashSet<String> {
        let mut out = HashSet::new();
        for p in perms {
            match self.flat_groups.get(p) {
                Some(set) => out.extend(set.iter().cloned()),
                None => {
                    out.insert(p.clone());
                }
            }
        }
        out
    }

    /// Whether `dynamic.http.anonymous_auth_enabled` is on.
    pub fn anonymous_enabled(&self) -> bool {
        self.dynamic
            .pointer("/dynamic/http/anonymous_auth_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Whether `do_not_fail_on_forbidden` is on.
    pub fn dnfof(&self) -> bool {
        self.dynamic
            .pointer("/dynamic/do_not_fail_on_forbidden")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }
}

// ---- persistence -----------------------------------------------------------

/// Where the configuration lives on disk: `config/security/*.yml`.
pub fn security_dir() -> PathBuf {
    crate::tls::config_dir().join("security")
}

/// The YAML of one kind, as the plugin writes it (with `_meta`).
fn document_yaml(kind: &str, body: &Value) -> String {
    let (ty, version) = match kind {
        "internalusers" => ("internalusers", 2),
        "roles" => ("roles", 2),
        "rolesmapping" => ("rolesmapping", 2),
        "actiongroups" => ("actiongroups", 2),
        "tenants" => ("tenants", 2),
        _ => ("config", 2),
    };
    let mut doc = Map::new();
    doc.insert("_meta".into(), json!({"type": ty, "config_version": version}));
    if let Some(o) = body.as_object() {
        for (k, v) in o {
            doc.insert(k.clone(), v.clone());
        }
    }
    serde_yaml::to_string(&Value::Object(doc)).unwrap_or_default()
}

impl SecurityConfig {
    /// The documents as the API and the files spell them.
    pub fn document(&self, kind: &str) -> Value {
        let mut o = Map::new();
        match kind {
            "internalusers" => {
                for (n, u) in &self.users {
                    let mut v = u.to_json();
                    v["hash"] = json!(u.hash);
                    o.insert(n.clone(), v);
                }
            }
            "roles" => {
                for (n, r) in &self.roles {
                    if !r.is_static {
                        o.insert(n.clone(), r.to_json());
                    }
                }
            }
            "rolesmapping" => {
                for (n, m) in &self.mappings {
                    o.insert(n.clone(), m.to_json());
                }
            }
            "actiongroups" => {
                for (n, g) in &self.action_groups {
                    if !g.is_static {
                        o.insert(n.clone(), g.to_json());
                    }
                }
            }
            "tenants" => {
                for (n, t) in &self.tenants {
                    if !t.is_static {
                        o.insert(n.clone(), t.to_json());
                    }
                }
            }
            "config" => return json!({"config": self.dynamic}),
            _ => {}
        }
        Value::Object(o)
    }

    /// Write every document to the security directory.
    pub fn save(&self) -> std::io::Result<()> {
        let dir = security_dir();
        std::fs::create_dir_all(&dir)?;
        for (kind, file) in [
            ("internalusers", "internal_users.yml"),
            ("roles", "roles.yml"),
            ("rolesmapping", "roles_mapping.yml"),
            ("actiongroups", "action_groups.yml"),
            ("tenants", "tenants.yml"),
            ("config", "config.yml"),
        ] {
            std::fs::write(dir.join(file), document_yaml(kind, &self.document(kind)))?;
        }
        Ok(())
    }

    /// The configuration on disk laid over the defaults, or the defaults
    /// alone where nothing was written yet.
    pub fn load() -> SecurityConfig {
        let mut c = SecurityConfig::defaults();
        let dir = security_dir();
        let mut docs = Vec::new();
        for (kind, file) in [
            ("internalusers", "internal_users.yml"),
            ("roles", "roles.yml"),
            ("rolesmapping", "roles_mapping.yml"),
            ("actiongroups", "action_groups.yml"),
            ("tenants", "tenants.yml"),
            ("config", "config.yml"),
        ] {
            if let Ok(text) = std::fs::read_to_string(dir.join(file)) {
                docs.push((kind, yaml_to_json(&text)));
            }
        }
        if !docs.is_empty() {
            // a file on disk is the whole of its kind, not an addition
            for (kind, _) in &docs {
                match *kind {
                    "internalusers" => c.users.clear(),
                    "rolesmapping" => c.mappings.clear(),
                    "roles" => c.roles.retain(|_, r| r.is_static),
                    "actiongroups" => c.action_groups.retain(|_, g| g.is_static),
                    "tenants" => c.tenants.retain(|_, t| t.is_static),
                    _ => {}
                }
            }
            c.merge_documents(&docs);
        }
        c
    }
}

// ---- wildcard matching -------------------------------------------------------

/// A pattern the plugin reads: `*` and `?` globs, `/regex/`, or a name.
pub fn pattern_matches(pattern: &str, candidate: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if pattern.len() > 2 && pattern.starts_with('/') && pattern.ends_with('/') {
        return regex::Regex::new(&pattern[1..pattern.len() - 1])
            .map(|re| re.is_match(candidate))
            .unwrap_or(false);
    }
    if pattern.contains('*') || pattern.contains('?') {
        return glob_matches(pattern, candidate);
    }
    pattern == candidate
}

fn glob_matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub fn any_matches(patterns: &[String], candidate: &str) -> bool {
    patterns.iter().any(|p| pattern_matches(p, candidate))
}

// ---- the caller ---------------------------------------------------------------

/// Who is asking: the user, the backend roles they came with, and the
/// roles the mappings gave them.
#[derive(Clone, Debug, Default)]
pub struct Caller {
    pub name: String,
    pub backend_roles: Vec<String>,
    pub attributes: BTreeMap<String, String>,
    /// the roles mapped for this caller, sorted
    pub roles: Vec<String>,
    pub remote_address: String,
    pub is_internal: bool,
    pub requested_tenant: Option<String>,
    /// whether security is off and this caller stands for everyone
    pub unrestricted: bool,
    /// authenticated by an admin client certificate
    pub admin_cert: bool,
}

impl Caller {
    /// The caller while security is off: allowed everything.
    pub fn unrestricted() -> Caller {
        Caller { name: "admin".into(), unrestricted: true, ..Caller::default() }
    }

    /// `User [name=…, backend_roles=[…], requestedTenant=null]`
    pub fn describe(&self) -> String {
        format!(
            "User [name={}, backend_roles=[{}], requestedTenant={}]",
            self.name,
            self.backend_roles.join(", "),
            self.requested_tenant.as_deref().unwrap_or("null")
        )
    }
}

/// `${user_name}` and the caller's attributes, written into a pattern.
pub fn substitute(pattern: &str, caller: &Caller) -> String {
    let mut out =
        pattern.replace("${user.name}", &caller.name).replace("${user_name}", &caller.name);
    if out.contains("${user.roles}") || out.contains("${user_roles}") {
        let quoted: Vec<String> = caller.backend_roles.iter().map(|r| format!("\"{r}\"")).collect();
        let joined = quoted.join(",");
        out = out.replace("${user.roles}", &joined).replace("${user_roles}", &joined);
    }
    if out.contains("${user.securityRoles}") || out.contains("${user_securityRoles}") {
        let quoted: Vec<String> = caller.roles.iter().map(|r| format!("\"{r}\"")).collect();
        let joined = quoted.join(",");
        out =
            out.replace("${user.securityRoles}", &joined).replace("${user_securityRoles}", &joined);
    }
    for (k, v) in &caller.attributes {
        out = out.replace(&format!("${{{k}}}"), v);
        out = out.replace(&format!("${{{}}}", k.replace('.', "_")), v);
    }
    out
}

impl SecurityConfig {
    /// The roles the mappings give a caller: by name, by backend role, by
    /// every one of a set of backend roles, or by host; plus the roles the
    /// user carries outright and the backend roles that are also role names.
    pub fn map_roles(
        &self,
        name: &str,
        backend_roles: &[String],
        security_roles: &[String],
        host: &str,
    ) -> Vec<String> {
        let mut out: HashSet<String> = security_roles.iter().cloned().collect();
        for (role, m) in &self.mappings {
            if any_matches(&m.users, name)
                || backend_roles.iter().any(|b| any_matches(&m.backend_roles, b))
                || (!m.and_backend_roles.is_empty()
                    && m.and_backend_roles
                        .iter()
                        .all(|p| backend_roles.iter().any(|b| pattern_matches(p, b))))
                || (!host.is_empty() && any_matches(&m.hosts, host))
            {
                out.insert(role.clone());
            }
        }
        java_set_order(out)
    }

    /// Whether a name is the wildcard-only `*` pattern.
    #[allow(dead_code)]
    fn is_any(p: &str) -> bool {
        p == "*"
    }

    /// Check a password against a user's bcrypt hash.
    pub fn authenticate(&self, name: &str, password: &str) -> Option<&InternalUser> {
        let user = self.users.get(name)?;
        if user.hash.is_empty() {
            return None;
        }
        // the plugin writes `$2y$`, which bcrypt reads as `$2b$`
        let hash = user.hash.replacen("$2y$", "$2b$", 1);
        bcrypt::verify(password, &hash).ok().filter(|ok| *ok).map(|_| user)
    }
}

/// Names in the order a Java `HashSet` would hand them back, which is the
/// order the plugin lists a caller's roles in: by hash bucket, then by
/// insertion (here: alphabetical) within a bucket.
pub fn java_set_order(names: HashSet<String>) -> Vec<String> {
    fn jhash(s: &str) -> u32 {
        s.encode_utf16().fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(c as u32))
    }
    let mut n: usize = 16;
    while (names.len() as f64) > n as f64 * 0.75 {
        n *= 2;
    }
    let mut sorted: Vec<String> = names.into_iter().collect();
    sorted.sort();
    let mut keyed: Vec<(usize, usize, String)> = sorted
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let h = jhash(&s);
            let spread = h ^ (h >> 16);
            ((spread as usize) & (n - 1), i, s)
        })
        .collect();
    keyed.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    keyed.into_iter().map(|(_, _, s)| s).collect()
}

/// Hash a password the way the plugin does: bcrypt, 12 rounds, `$2y$`.
pub fn hash_password(password: &str) -> String {
    bcrypt::hash(password, 12).map(|h| h.replacen("$2b$", "$2y$", 1)).unwrap_or_default()
}

// ---- privileges ---------------------------------------------------------------

/// Whether an action is decided at the cluster level.
pub fn is_cluster_action(action: &str) -> bool {
    action.starts_with("cluster:")
        || action.starts_with("indices:admin/template/")
        || action.starts_with("indices:admin/index_template/")
        || action.starts_with("indices:data/read/scroll")
        || action == "indices:data/write/bulk"
        || action == "indices:data/read/mget"
        || action.starts_with("indices:data/read/msearch")
        || action == "indices:data/read/mtv"
        || action == "indices:data/write/reindex"
        || action == "cluster:admin/scripts/painless/execute"
}

/// What an evaluation said.
#[derive(Clone, Debug)]
pub enum Verdict {
    Allowed,
    /// allowed only for these of the requested indices (do_not_fail_on_forbidden)
    Partial(Vec<String>),
    Denied {
        missing: String,
    },
}

impl SecurityConfig {
    fn action_allowed(&self, perms: &[String], action: &str) -> bool {
        self.resolve_actions(perms).iter().any(|p| pattern_matches(p, action))
    }

    /// Whether the caller may run a cluster-level action.
    pub fn cluster_allowed(&self, caller: &Caller, action: &str) -> bool {
        if caller.unrestricted {
            return true;
        }
        caller.roles.iter().filter_map(|r| self.roles.get(r)).any(|role| {
            self.action_allowed(&role.cluster_permissions, action)
                // a role's index permissions over `*` with `*` allowed cover
                // the cluster too, the way all_access is written
                || false
        })
    }

    /// Whether the caller may run an index-level action on these indices.
    pub fn index_verdict(&self, caller: &Caller, action: &str, indices: &[String]) -> Verdict {
        if caller.unrestricted {
            return Verdict::Allowed;
        }
        let roles: Vec<&Role> = caller.roles.iter().filter_map(|r| self.roles.get(r)).collect();
        // a role granting the action over every index grants it whatever
        // the indices are, even ones not there yet
        let wildcard = roles.iter().any(|role| {
            role.index_permissions.iter().any(|p| {
                p.index_patterns.iter().any(|pat| substitute(pat, caller) == "*")
                    && self.action_allowed(&p.allowed_actions, action)
            })
        });
        if wildcard {
            return Verdict::Allowed;
        }
        if indices.is_empty() {
            return Verdict::Allowed;
        }
        let mut granted: Vec<String> = Vec::new();
        for index in indices {
            let ok = roles.iter().any(|role| {
                role.index_permissions.iter().any(|p| {
                    p.index_patterns
                        .iter()
                        .any(|pat| pattern_matches(&substitute(pat, caller), index))
                        && self.action_allowed(&p.allowed_actions, action)
                })
            });
            if ok {
                granted.push(index.clone());
            }
        }
        if granted.len() == indices.len() {
            Verdict::Allowed
        } else if !granted.is_empty() && self.dnfof() {
            Verdict::Partial(granted)
        } else {
            Verdict::Denied { missing: action.to_string() }
        }
    }

    /// The document-level filters and field rules that apply to a caller on
    /// one index, from every role that reaches it.
    pub fn restrictions(&self, caller: &Caller, index: &str) -> IndexRestrictions {
        let mut out = IndexRestrictions::default();
        if caller.unrestricted {
            return out;
        }
        for role in caller.roles.iter().filter_map(|r| self.roles.get(r)) {
            for p in &role.index_permissions {
                if !p
                    .index_patterns
                    .iter()
                    .any(|pat| pattern_matches(&substitute(pat, caller), index))
                {
                    continue;
                }
                out.reached = true;
                match &p.dls {
                    Some(q) => out.dls.push(substitute(q, caller)),
                    None => out.unfiltered = true,
                }
                if p.fls.is_empty() {
                    out.unrestricted_fields = true;
                } else {
                    out.fls.push(p.fls.clone());
                }
                if p.masked_fields.is_empty() {
                    out.unmasked = true;
                } else {
                    out.masked.push(p.masked_fields.clone());
                }
            }
        }
        out
    }
}

/// What a caller's roles say about one index's documents and fields.
#[derive(Clone, Debug, Default)]
pub struct IndexRestrictions {
    pub reached: bool,
    /// the DLS queries, any of which lets a document through
    pub dls: Vec<String>,
    /// some role reaching the index has no DLS: nothing is filtered
    pub unfiltered: bool,
    pub fls: Vec<Vec<String>>,
    pub unrestricted_fields: bool,
    pub masked: Vec<Vec<String>>,
    pub unmasked: bool,
}

impl IndexRestrictions {
    /// The one query the caller's view is filtered by, if any.
    pub fn dls_query(&self) -> Option<Value> {
        if self.unfiltered || self.dls.is_empty() {
            return None;
        }
        let parsed: Vec<Value> =
            self.dls.iter().filter_map(|q| serde_json::from_str(q).ok()).collect();
        if parsed.is_empty() {
            return None;
        }
        if parsed.len() == 1 {
            return parsed.into_iter().next();
        }
        Some(json!({"bool": {"should": parsed, "minimum_should_match": 1}}))
    }

    /// Whether a field may be seen: every role reaching the index must
    /// allow it (a role without FLS allows all).
    pub fn field_visible(&self, field: &str) -> bool {
        if self.unrestricted_fields || self.fls.is_empty() {
            return true;
        }
        self.fls.iter().any(|rules| fls_allows(rules, field))
    }

    /// Whether a field's value is masked for this caller.
    pub fn field_masked(&self, field: &str) -> bool {
        if self.unmasked || self.masked.is_empty() {
            return false;
        }
        self.masked
            .iter()
            .any(|rules| rules.iter().any(|r| pattern_matches(masked_pattern(r), field)))
    }
}

/// A masked field rule may carry an algorithm after `::`.
pub fn masked_pattern(rule: &str) -> &str {
    rule.split("::").next().unwrap_or(rule)
}

/// FLS rules: `~field` excludes; without any exclusion the list is what is
/// allowed. A field under an allowed object is allowed with it.
pub fn fls_allows(rules: &[String], field: &str) -> bool {
    let excludes: Vec<&str> = rules.iter().filter_map(|r| r.strip_prefix('~')).collect();
    let includes: Vec<&String> = rules.iter().filter(|r| !r.starts_with('~')).collect();
    if !excludes.is_empty() {
        let excluded = excludes
            .iter()
            .any(|e| pattern_matches(e, field) || field.starts_with(&format!("{e}.")));
        if excluded {
            return false;
        }
        if includes.is_empty() {
            return true;
        }
    }
    includes.iter().any(|i| {
        pattern_matches(i, field)
            || field.starts_with(&format!("{i}."))
            // asking for a parent object whose children are allowed
            || i.starts_with(&format!("{field}."))
    })
}

/// The security state the server holds: the configuration, and whether it
/// is switched on.
pub struct Security {
    pub enabled: bool,
    /// Callers already checked, by a digest of what they presented. bcrypt
    /// is made to be slow, and the plugin checks a password once and keeps
    /// the user for `plugins.security.cache.ttl_minutes`; so does this.
    auth_cache: parking_lot::Mutex<HashMap<[u8; 32], CachedCaller>>,
    /// bumped on every configuration change, which empties the cache
    generation: std::sync::atomic::AtomicU64,
    cache_ttl: std::time::Duration,
    /// the authentication domains and authorizers `config.yml` names
    pub chain: RwLock<Arc<authc::AuthChain>>,
    chain_state: authc::ChainState,
    /// `plugins.security.authcz.admin_dn`: certificates that are the admin
    admin_dns: Vec<String>,
    pub config: RwLock<SecurityConfig>,
    /// the roles that may use the security REST API
    pub restapi_roles: Vec<String>,
    /// `plugins.security.compliance.salt`, for field masking
    pub salt: String,
}

impl Security {
    pub fn from_settings(settings: &Value) -> Arc<Security> {
        let get = |k: &str| crate::tls::node_setting(settings, k);
        let disabled = get("plugins.security.disabled").map(|v| v != "false").unwrap_or(true);
        let config = if disabled { SecurityConfig::defaults() } else { SecurityConfig::load() };
        let restapi_roles = get("plugins.security.restapi.roles_enabled")
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().trim_matches(['[', ']', '"', '\'']).to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_else(|| {
                vec!["all_access".to_string(), "security_rest_api_access".to_string()]
            });
        let ttl_minutes = get("plugins.security.cache.ttl_minutes")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60);
        let admin_dns: Vec<String> = match settings
            .pointer("/plugins/security/authcz/admin_dn")
            .or_else(|| settings.get("plugins.security.authcz.admin_dn"))
        {
            Some(Value::Array(a)) => {
                a.iter().filter_map(|v| v.as_str()).map(normalize_dn).collect()
            }
            Some(Value::String(one)) => vec![normalize_dn(one)],
            _ => get("plugins.security.authcz.admin_dn")
                .map(|v| {
                    v.split(';')
                        .map(|s| normalize_dn(s.trim().trim_matches(['[', ']', '"', '\''])))
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
        };
        let chain = Arc::new(authc::AuthChain::from_dynamic(&config.dynamic));
        Arc::new(Security {
            enabled: !disabled,
            auth_cache: parking_lot::Mutex::new(HashMap::new()),
            generation: std::sync::atomic::AtomicU64::new(0),
            cache_ttl: std::time::Duration::from_secs(ttl_minutes * 60),
            chain: RwLock::new(chain),
            chain_state: authc::ChainState::new(std::time::Duration::from_secs(ttl_minutes * 60)),
            admin_dns,
            config: RwLock::new(config),
            restapi_roles,
            salt: get("plugins.security.compliance.salt")
                .unwrap_or_else(|| "e1ukloTsQlOgPquJ".into()),
        })
    }

    /// The caller a request stands for, from its basic-auth header.
    pub fn caller_from_basic(
        &self,
        header: Option<&str>,
        remote: &str,
    ) -> Result<Caller, AuthFailure> {
        if !self.enabled {
            return Ok(Caller::unrestricted());
        }
        let cfg = self.config.read();
        let Some(h) = header else {
            if cfg.anonymous_enabled() {
                return Ok(self.anonymous(&cfg, remote));
            }
            return Err(AuthFailure::Challenge);
        };
        let Some(encoded) = h.strip_prefix("Basic ").or_else(|| h.strip_prefix("basic ")) else {
            return Err(AuthFailure::Challenge);
        };
        use base64::Engine;
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
            return Err(AuthFailure::Challenge);
        };
        let text = String::from_utf8_lossy(&bytes).to_string();
        let Some((name, password)) = text.split_once(':') else {
            return Err(AuthFailure::Challenge);
        };
        let key = credential_digest(name, password, remote);
        let generation = self.generation.load(std::sync::atomic::Ordering::Acquire);
        if let Some(hit) = self.auth_cache.lock().get(&key) {
            if hit.generation == generation && hit.at.elapsed() < self.cache_ttl {
                return Ok(hit.caller.clone());
            }
        }
        let Some(user) = cfg.authenticate(name, password) else { return Err(AuthFailure::Failed) };
        let roles = cfg.map_roles(name, &user.backend_roles, &user.security_roles, remote);
        let caller = Caller {
            name: name.to_string(),
            backend_roles: user.backend_roles.clone(),
            attributes: user.attributes.clone(),
            roles,
            remote_address: remote.to_string(),
            is_internal: true,
            requested_tenant: None,
            unrestricted: false,
            admin_cert: false,
        };
        let mut cache = self.auth_cache.lock();
        if cache.len() > 10_000 {
            cache.clear();
        }
        cache.insert(
            key,
            CachedCaller { generation, at: std::time::Instant::now(), caller: caller.clone() },
        );
        Ok(caller)
    }

    /// The configuration changed: nothing already checked still holds.
    ///
    /// The caller passes the configuration it holds: this is called from
    /// under the configuration's write lock, which must not be taken again.
    pub fn touch(&self, cfg: &SecurityConfig) {
        self.generation.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.auth_cache.lock().clear();
        self.chain_state.clear();
        let chain = Arc::new(authc::AuthChain::from_dynamic(&cfg.dynamic));
        *self.chain.write() = chain;
    }

    /// The caller a request stands for, by every domain `config.yml`
    /// names, remembered by what it presented for `cache.ttl_minutes`.
    pub async fn caller_for(
        &self,
        presented: &authc::Presented<'_>,
    ) -> Result<Caller, authc::Refusal> {
        if !self.enabled {
            return Ok(Caller::unrestricted());
        }
        // an admin certificate is the admin, before any domain is asked
        if let Some(dn) = &presented.peer_dn {
            if self.admin_dns.iter().any(|a| *a == normalize_dn(dn)) {
                let mut c = Caller::unrestricted();
                c.name = dn.clone();
                c.admin_cert = true;
                c.remote_address = presented.remote.clone();
                return Ok(c);
            }
        }
        let key = presented_digest(presented);
        let generation = self.generation.load(std::sync::atomic::Ordering::Acquire);
        if let Some(hit) = self.auth_cache.lock().get(&key) {
            if hit.generation == generation && hit.at.elapsed() < self.cache_ttl {
                return Ok(hit.caller.clone());
            }
        }
        let chain = self.chain.read().clone();
        // a snapshot of the configuration: the chain awaits on LDAP and the
        // network, and no lock may be held across that
        let cfg: Arc<SecurityConfig> = Arc::new(self.config.read().clone());
        let caller = chain.authenticate(&cfg, &self.chain_state, presented).await?;
        let mut cache = self.auth_cache.lock();
        if cache.len() > 10_000 {
            cache.clear();
        }
        cache.insert(
            key,
            CachedCaller { generation, at: std::time::Instant::now(), caller: caller.clone() },
        );
        Ok(caller)
    }

    fn anonymous(&self, cfg: &SecurityConfig, remote: &str) -> Caller {
        let roles = cfg.map_roles(
            "opendistro_security_anonymous",
            &["opendistro_security_anonymous_backendrole".to_string()],
            &[],
            remote,
        );
        Caller {
            name: "opendistro_security_anonymous".into(),
            backend_roles: vec!["opendistro_security_anonymous_backendrole".into()],
            roles,
            remote_address: remote.to_string(),
            ..Caller::default()
        }
    }

    /// Whether the caller may use the security REST API.
    pub fn may_administer(&self, caller: &Caller) -> bool {
        caller.unrestricted || caller.roles.iter().any(|r| self.restapi_roles.contains(r))
    }
}

/// A DN with the spaces after its commas dropped, for comparing.
pub fn normalize_dn(dn: &str) -> String {
    dn.split(',').map(|p| p.trim()).collect::<Vec<_>>().join(",")
}

/// Everything a request presents that could tell who it is, digested.
fn presented_digest(p: &authc::Presented<'_>) -> [u8; 32] {
    use sha2::Digest as _;
    static NONCE: std::sync::OnceLock<[u8; 16]> = std::sync::OnceLock::new();
    let nonce = NONCE.get_or_init(|| {
        let mut n = [0u8; 16];
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        n.copy_from_slice(&t.to_le_bytes());
        n
    });
    let mut h = sha2::Sha256::new();
    h.update(nonce);
    for name in ["authorization", "x-proxy-user", "x-proxy-roles", "x-forwarded-for"] {
        if let Some(v) = p.headers.get(name) {
            h.update(name.as_bytes());
            h.update([0u8]);
            h.update(v.as_bytes());
            h.update([0u8]);
        }
    }
    // every header is part of it: a jwt_header or user_header may be anything
    let mut names: Vec<String> = p.headers.keys().map(|k| k.as_str().to_string()).collect();
    names.sort();
    for n in names {
        if [
            "accept",
            "content-type",
            "content-length",
            "user-agent",
            "host",
            "connection",
            "accept-encoding",
        ]
        .contains(&n.as_str())
        {
            continue;
        }
        if let Some(v) = p.headers.get(&n) {
            h.update(n.as_bytes());
            h.update([1u8]);
            h.update(v.as_bytes());
            h.update([1u8]);
        }
    }
    h.update(p.query.as_bytes());
    h.update([2u8]);
    h.update(p.remote.as_bytes());
    h.update([3u8]);
    if let Some(dn) = &p.peer_dn {
        h.update(dn.as_bytes());
    }
    h.finalize().into()
}

/// A caller the cache holds, and which configuration it was checked against.
#[derive(Clone)]
struct CachedCaller {
    generation: u64,
    at: std::time::Instant,
    caller: Caller,
}

/// What a caller presented, digested so that the cache never holds a
/// password; a per-process nonce keeps the digests from being useful
/// anywhere else.
fn credential_digest(name: &str, password: &str, remote: &str) -> [u8; 32] {
    use sha2::Digest as _;
    static NONCE: std::sync::OnceLock<[u8; 16]> = std::sync::OnceLock::new();
    let nonce = NONCE.get_or_init(|| {
        let mut n = [0u8; 16];
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        n[..16].copy_from_slice(&t.to_le_bytes());
        let pid = std::process::id().to_le_bytes();
        for (i, b) in pid.iter().enumerate() {
            n[i] ^= *b;
        }
        n
    });
    let mut h = sha2::Sha256::new();
    h.update(nonce);
    h.update(name.as_bytes());
    h.update([0u8]);
    h.update(password.as_bytes());
    h.update([0u8]);
    h.update(remote.as_bytes());
    h.finalize().into()
}

/// Why a request could not be authenticated.
#[derive(Debug)]
pub enum AuthFailure {
    /// no usable credentials: answer 401 with a challenge
    Challenge,
    /// credentials given but wrong
    Failed,
}

// ---- the caller's view, for the paths that read documents ---------------------

/// The DLS query the current caller's view of an index is filtered by, if
/// security is on and their roles filter it. Nothing is read while
/// security is off.
pub fn dls_for(store: &crate::store::Store, index: &str) -> Option<Value> {
    if !store.security.enabled {
        return None;
    }
    let caller = layer::current_caller()?;
    if caller.unrestricted {
        return None;
    }
    let cfg = store.security.config.read();
    cfg.restrictions(&caller, index).dls_query()
}

/// The caller's field rules for an index, if security is on.
pub fn restrictions_for(store: &crate::store::Store, index: &str) -> Option<IndexRestrictions> {
    if !store.security.enabled {
        return None;
    }
    let caller = layer::current_caller()?;
    if caller.unrestricted {
        return None;
    }
    let cfg = store.security.config.read();
    let r = cfg.restrictions(&caller, index);
    if !r.reached {
        return None;
    }
    Some(r)
}

/// A query JSON with the caller's DLS folded in as a filter.
pub fn with_dls(store: &crate::store::Store, index: &str, query: Option<Value>) -> Option<Value> {
    let Some(dls) = dls_for(store, index) else { return query };
    let base = query.unwrap_or_else(|| json!({"match_all": {}}));
    Some(json!({"bool": {"must": [base], "filter": [dls]}}))
}

/// Whether one document is inside the caller's view of its index.
pub fn doc_visible(store: &crate::store::Store, g: &crate::store::IdxState, id: &str) -> bool {
    let Some(dls) = dls_for(store, &g.name) else { return true };
    use boostcore::collector::Count;
    use boostcore::query::{BooleanQuery, Occur, TermQuery};
    use boostcore::schema::IndexRecordOption;
    let searcher = g.reader.searcher();
    let ctx = crate::query::Ctx {
        fields: &g.fields,
        mapping: &g.mapping,
        analysis: &g.analysis,
        index: &g.index,
        max_terms_count: g.max_terms_count(),
        max_regex_length: g.max_regex_length(),
        allow_expensive: true,
        observed_kinds: &g.observed_kinds,
        kinds_complete: g.kinds_complete,
        stats: &g.stats,
    };
    let Ok(filter) = crate::query::build(&ctx, &dls) else { return false };
    let probe =
        TermQuery::new(boostcore::Term::from_field_text(g.fields.id, id), IndexRecordOption::Basic);
    let q = BooleanQuery::new(vec![
        (Occur::Must, Box::new(probe) as Box<dyn boostcore::query::Query>),
        (Occur::Must, filter),
    ]);
    searcher.search(&q, &Count).map(|n| n > 0).unwrap_or(false)
}

/// A document's source as the caller may see it (hidden fields gone,
/// masked ones hashed); the source untouched while security is off.
pub fn narrow_source(store: &crate::store::Store, index: &str, src: &mut Value) {
    if let Some(view) = view::view_for(store, index) {
        view.filter_source(src);
    }
}

/// Term vectors as the caller may see them: hidden fields gone, the terms
/// of masked fields hashed.
pub fn narrow_term_vectors(store: &crate::store::Store, index: &str, fields: &mut Value) {
    let Some(view) = view::view_for(store, index) else { return };
    let Some(o) = fields.as_object_mut() else { return };
    let names: Vec<String> = o.keys().cloned().collect();
    for name in names {
        if view.hidden(&name) {
            o.remove(&name);
        } else if view.masked(&name) {
            if let Some(Value::Object(terms)) = o.get_mut(&name).and_then(|f| f.get_mut("terms")) {
                let raw: Vec<(String, Value)> =
                    terms.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                terms.clear();
                for (k, v) in raw {
                    terms.insert(view.mask_text(&k), v);
                }
            }
        }
    }
}

/// Whether the caller may run every one of these actions over these
/// indices; the refusal names the whole list, as the plugin's shard-level
/// check does for a bulk, an mget or an msearch item.
pub fn item_refusal(
    store: &crate::store::Store,
    actions: &[&str],
    indices: &[String],
) -> Option<String> {
    if !store.security.enabled {
        return None;
    }
    let caller = layer::current_caller()?;
    if caller.unrestricted {
        return None;
    }
    let cfg = store.security.config.read();
    let denied = actions
        .iter()
        .any(|a| matches!(cfg.index_verdict(&caller, a, indices), Verdict::Denied { .. }));
    if !denied {
        return None;
    }
    Some(format!("no permissions for [{}] and {}", actions.join(", "), caller.describe()))
}

/// A `security_exception` body for one item of a many-item request.
pub fn item_error(reason: &str) -> Value {
    json!({
        "root_cause": [{"type": "security_exception", "reason": reason}],
        "type": "security_exception",
        "reason": reason,
    })
}

/// The single-logout URL for a caller that came in through SAML, if the
/// chain has a SAML domain and the IdP has a logout service.
pub fn sso_logout_url(security: &Security, caller: &Caller) -> Option<String> {
    let chain = security.chain.read().clone();
    let saml = chain.domains.iter().find_map(|d| match &d.authenticator {
        authc::Authenticator::Saml(s, _) => Some(s.clone()),
        _ => None,
    })?;
    let came_by_saml = caller.attributes.contains_key("attr.jwt.saml_nif")
        || caller.attributes.contains_key("attr.jwt.saml_si");
    if !came_by_saml {
        return None;
    }
    let name_id =
        caller.attributes.get("attr.jwt.saml_ni").cloned().unwrap_or_else(|| caller.name.clone());
    saml.logout_url(
        &name_id,
        caller.attributes.get("attr.jwt.saml_nif").map(|s| s.as_str()),
        caller.attributes.get("attr.jwt.saml_si").map(|s| s.as_str()),
    )
}
