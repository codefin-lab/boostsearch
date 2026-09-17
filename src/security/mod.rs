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
pub mod audit;
pub mod authc;
pub mod layer;
pub mod obo;
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
    /// Which change this is: one more for every change made through the
    /// security API. The cluster manager's configuration is the cluster's,
    /// and a manager newly elected over a configuration newer than the one
    /// it holds takes that one rather than handing the cluster its own.
    pub generation: u64,
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
    /// What every configuration starts from: the plugin's static roles,
    /// action groups and tenants, and its default roles, mappings and
    /// authentication chain -- with no user in it.
    ///
    /// The plugin's demo users are not here. Their passwords are published,
    /// and a node that fell back to them let `admin:admin` in wherever
    /// security was switched on and nothing had been configured yet.
    pub fn builtin() -> SecurityConfig {
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
            ("roles", yaml_to_json(include_str!("defaults/roles.yml"))),
            ("rolesmapping", yaml_to_json(include_str!("defaults/roles_mapping.yml"))),
            ("actiongroups", yaml_to_json(include_str!("defaults/action_groups.yml"))),
            ("tenants", yaml_to_json(include_str!("defaults/tenants.yml"))),
            ("config", yaml_to_json(include_str!("defaults/config.yml"))),
        ]);
        c
    }

    /// The first configuration of a node, from the administrator's password
    /// the operator gave it: the built-in configuration and one user, `admin`,
    /// mapped to `all_access` through its backend role as the plugin's demo
    /// installer maps it.
    pub fn seeded(admin_password: &str) -> SecurityConfig {
        let mut c = SecurityConfig::builtin();
        c.users.insert(
            "admin".into(),
            InternalUser::from_json(&json!({
                "hash": hash_password(admin_password),
                "reserved": true,
                "backend_roles": ["admin"],
                "description": "Administrator, from the initial admin password",
            })),
        );
        c.generation = 1;
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

/// The files a configuration is written to, by the kind each one holds.
const FILES: [(&str, &str); 6] = [
    ("internalusers", "internal_users.yml"),
    ("roles", "roles.yml"),
    ("rolesmapping", "roles_mapping.yml"),
    ("actiongroups", "action_groups.yml"),
    ("tenants", "tenants.yml"),
    ("config", "config.yml"),
];

/// The generation of the files beside it, one number.
const GENERATION_FILE: &str = "generation";

/// Present while a save is being put in place: every file of the new
/// generation is on disk whole beside the one it replaces, and only the
/// renaming is left to do.
const PENDING: &str = ".pending";

fn tmp_path(dir: &std::path::Path, file: &str) -> PathBuf {
    dir.join(format!("{file}.tmp"))
}

/// Every file a save writes, the generation last.
fn saved_files() -> impl Iterator<Item = &'static str> {
    FILES.iter().map(|(_, f)| *f).chain(std::iter::once(GENERATION_FILE))
}

fn write_synced(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    crate::store::sync_file(&f)
}

/// A rename is only as durable as the directory that records it.
fn sync_dir(dir: &std::path::Path) -> std::io::Result<()> {
    crate::store::sync_file(&std::fs::File::open(dir)?)
}

/// Put a decided generation in place: what is still waiting beside its file
/// replaces it, and the marker goes last.
fn finish_save(dir: &std::path::Path) -> std::io::Result<()> {
    for file in saved_files() {
        let tmp = tmp_path(dir, file);
        if tmp.exists() {
            std::fs::rename(&tmp, dir.join(file))?;
        }
    }
    sync_dir(dir)?;
    std::fs::remove_file(dir.join(PENDING))?;
    sync_dir(dir)
}

/// What a save that was stopped part way left behind, dealt with: finished
/// when it had got as far as deciding, forgotten when it had not. Either way
/// the files read afterwards are all of one generation.
fn recover_save(dir: &std::path::Path) -> std::io::Result<()> {
    if dir.join(PENDING).exists() {
        return finish_save(dir);
    }
    for file in saved_files() {
        match std::fs::remove_file(tmp_path(dir, file)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// A document as the files hold it, or why it is not one.
fn parse_document(file: &str, text: &str) -> Result<Value, String> {
    let yaml: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|e| format!("{file} is not valid YAML: {e}"))?;
    match serde_json::to_value(yaml) {
        Ok(v @ Value::Object(_)) => Ok(v),
        // a file with nothing in it is a kind with nothing in it
        Ok(Value::Null) => Ok(Value::Object(Map::new())),
        _ => Err(format!("{file} does not hold a mapping")),
    }
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

    /// Write the whole configuration to the security directory, as one
    /// generation or not at all.
    ///
    /// Six files written one after another in place were six chances for a
    /// failure -- a full disk, a directory the node may not write, the
    /// process stopped -- to leave users of one generation beside mappings
    /// of another. Every file of the new generation is written out and
    /// synced beside the old one first; a marker then says the new
    /// generation is decided, and only after that are the files renamed
    /// into place. A load finishes a decided save and forgets an undecided
    /// one, so what it reads is always one generation.
    pub fn save(&self) -> std::io::Result<()> {
        self.save_in(&security_dir())
    }

    pub fn save_in(&self, dir: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        recover_save(dir)?;
        let decided = self.write_undecided(dir).and_then(|_| {
            write_synced(&dir.join(PENDING), b"")?;
            sync_dir(dir)
        });
        if let Err(e) = decided {
            // nothing was decided: the old generation stands, and what was
            // written of the new one is taken away again
            let _ = std::fs::remove_file(dir.join(PENDING));
            let _ = recover_save(dir);
            return Err(e);
        }
        finish_save(dir)
    }

    /// Every file of this generation, written and synced beside the files it
    /// is to replace.
    fn write_undecided(&self, dir: &std::path::Path) -> std::io::Result<()> {
        for (kind, file) in FILES {
            let yaml = document_yaml(kind, &self.document(kind));
            write_synced(&tmp_path(dir, file), yaml.as_bytes())?;
        }
        write_synced(&tmp_path(dir, GENERATION_FILE), format!("{}\n", self.generation).as_bytes())?;
        sync_dir(dir)
    }

    /// The built-in configuration with these documents laid over it, each
    /// document being the whole of its kind rather than an addition to it.
    pub fn from_documents(docs: &[(&str, Value)]) -> SecurityConfig {
        let mut c = SecurityConfig::builtin();
        for (kind, _) in docs {
            match *kind {
                "internalusers" => c.users.clear(),
                "rolesmapping" => c.mappings.clear(),
                "roles" => c.roles.retain(|_, r| r.is_static),
                "actiongroups" => c.action_groups.retain(|_, g| g.is_static),
                "tenants" => c.tenants.retain(|_, t| t.is_static),
                _ => {}
            }
        }
        c.merge_documents(docs);
        c
    }

    /// The configuration on disk: `None` where nothing was ever written, and
    /// an error where something was and cannot be read whole.
    ///
    /// A file that could not be read used to be skipped and the defaults
    /// stood in for it, demo users and their published passwords among them:
    /// a users file the node lost permission to read brought back `admin`
    /// with the password `admin`. A configuration is all six files or it is
    /// not one.
    pub fn load() -> Result<Option<SecurityConfig>, String> {
        SecurityConfig::load_from(&security_dir())
    }

    pub fn load_from(dir: &std::path::Path) -> Result<Option<SecurityConfig>, String> {
        recover_save(dir).map_err(|e| {
            format!(
                "an interrupted save of the security configuration in {} could not be completed: {e}",
                dir.display()
            )
        })?;
        let mut docs = Vec::new();
        let mut missing = Vec::new();
        for (kind, file) in FILES {
            match std::fs::read_to_string(dir.join(file)) {
                Ok(text) => docs.push((kind, parse_document(file, &text)?)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => missing.push(file),
                Err(e) => return Err(format!("{}: {e}", dir.join(file).display())),
            }
        }
        if docs.is_empty() {
            return Ok(None);
        }
        if !missing.is_empty() {
            return Err(format!(
                "the security configuration in {} is incomplete: {} missing",
                dir.display(),
                missing.join(", ")
            ));
        }
        let path = dir.join(GENERATION_FILE);
        let generation = match std::fs::read_to_string(&path) {
            Ok(text) => text
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("{} does not hold a number", path.display()))?,
            // written by hand, or before generations were kept
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let mut c = SecurityConfig::from_documents(&docs);
        c.generation = generation;
        Ok(Some(c))
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
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
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

    /// Whether this is a service account: an internal user whose `service`
    /// attribute is `true`. The plugin allows such an account no cluster
    /// action at all, whatever its roles say.
    pub fn is_service_account(&self) -> bool {
        self.is_internal && self.attributes.get("service").map(|v| v == "true").unwrap_or(false)
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
    keyed.sort_by_key(|a| (a.0, a.1));
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

    /// Whether the caller may read, or write, in a Dashboards tenant, as the
    /// plugin's tenant privileges answer it: a role's tenant patterns reach
    /// only tenants that are defined, `kibana_all_write` (anything resolving
    /// to `kibana:saved_objects/*/write`) is both read and write, anything
    /// else is read. A caller with `kibana_user` and no read access to the
    /// global tenant is given it anyway, which the plugin keeps for old
    /// configurations.
    pub fn tenant_privilege(&self, caller: &Caller, tenant: &str, write: bool) -> bool {
        if caller.unrestricted {
            return true;
        }
        if !self.tenants.contains_key(tenant) {
            return false;
        }
        let granted = |write: bool| {
            caller.roles.iter().filter_map(|r| self.roles.get(r)).any(|role| {
                role.tenant_permissions.iter().any(|tp| {
                    let writes = self
                        .resolve_actions(&tp.allowed_actions)
                        .contains("kibana:saved_objects/*/write");
                    (writes || !write)
                        && tp
                            .tenant_patterns
                            .iter()
                            .any(|p| pattern_matches(&substitute(p, caller), tenant))
                })
            })
        };
        if granted(write) {
            return true;
        }
        tenant == "global_tenant"
            && caller.roles.iter().any(|r| r == "kibana_user")
            && !granted(false)
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
        // A filter that cannot be read is not a filter that does not apply.
        // Dropping the ones that would not parse, and answering `None` when
        // none of them did, handed the caller every document in the index --
        // the restriction disappeared instead of the request being refused.
        // One that cannot be read now matches nothing.
        let mut parsed: Vec<Value> = Vec::with_capacity(self.dls.len());
        for q in &self.dls {
            match serde_json::from_str(q) {
                Ok(v) => parsed.push(v),
                Err(_) => {
                    tracing::error!(
                        "a document-level filter could not be read; the caller is shown nothing"
                    );
                    return Some(json!({"bool": {"must_not": [{"match_all": {}}]}}));
                }
            }
        }
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

/// What a node with security on starts with: its saved configuration; or,
/// where it has none, one seeded from the initial admin password; or nothing,
/// and nobody is let in until it is given one.
fn first_configuration(refusal: &mut Option<String>) -> Option<SecurityConfig> {
    match SecurityConfig::load() {
        Ok(Some(c)) => Some(c),
        Err(why) => {
            tracing::error!("{why}; nobody is let in until it is put right");
            eprintln!("velosearch: {why}; nobody is let in until it is put right");
            None
        }
        Ok(None) => match initial_admin_password() {
            Ok(Some(password)) => {
                let c = SecurityConfig::seeded(&password);
                match c.save() {
                    Ok(()) => Some(c),
                    Err(e) => {
                        *refusal = Some(format!(
                            "the security configuration seeded from {INITIAL_ADMIN_PASSWORD} could not be saved in {}: {e}",
                            security_dir().display()
                        ));
                        None
                    }
                }
            }
            Ok(None) => {
                eprintln!(
                    "velosearch: security is on and {} holds no configuration; nobody is let in \
                     until one is saved there, {INITIAL_ADMIN_PASSWORD} is set, or the cluster \
                     manager provides one",
                    security_dir().display()
                );
                None
            }
            Err(why) => {
                *refusal = Some(why);
                None
            }
        },
    }
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
    /// the audit log, and the sink it writes to
    pub audit: Arc<audit::AuditLog>,
    pub config: RwLock<SecurityConfig>,
    /// the roles that may use the security REST API
    pub restapi_roles: Vec<String>,
    /// `plugins.security.restapi.endpoints_disabled.<role>.<ENDPOINT>`: the
    /// methods a role may not use on an endpoint of the security API. It was
    /// read by nothing at all, so an operator delegating read-only access to
    /// the API delegated everything.
    pub endpoints_disabled: HashMap<String, HashMap<String, Vec<String>>>,
    /// whether `PATCH /_plugins/_security/api/securityconfig` may rewrite the
    /// authentication chain. Off unless an operator turns it on, as the
    /// reference has it: the payload *is* the chain, so anyone who may write
    /// it can add a domain that authenticates a header they choose.
    pub allow_config_rewrite: bool,
    /// `plugins.security.compliance.salt`, for field masking
    pub salt: String,
    /// whether the node holds a configuration at all: read from its files,
    /// seeded from the initial admin password, or taken from the cluster
    configured: std::sync::atomic::AtomicBool,
    /// whether the configuration held is one to let anybody in by. A node on
    /// its own is ready once it holds one; a node of a cluster only once it
    /// holds the one its cluster manager published.
    ready: std::sync::atomic::AtomicBool,
    /// whether this node is one of a cluster rather than a cluster of itself
    clustered: std::sync::atomic::AtomicBool,
    /// the configuration as the cluster state carries it, by the change it
    /// was made at
    wire: parking_lot::Mutex<Option<(u64, Value)>>,
    /// why the node must not start: an initial admin password too weak to be
    /// one, or a configuration seeded from it that could not be saved
    pub refusal: Option<String>,
}

/// Whether a node may let anybody in by the configuration it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    Ready,
    /// no configuration, or not yet the cluster's
    NotInitialized,
    /// the cluster's, as of when this node last had a cluster manager: what
    /// has been revoked since, it cannot know
    NoManager,
}

/// The environment variable an operator gives the first administrator's
/// password in, as OpenSearch's `OPENSEARCH_INITIAL_ADMIN_PASSWORD`.
pub const INITIAL_ADMIN_PASSWORD: &str = "VELOSEARCH_INITIAL_ADMIN_PASSWORD";

/// The initial admin password, if one was given, or why it cannot be one.
///
/// OpenSearch's installer refuses a password that is not at least eight
/// characters with an uppercase letter, a lowercase letter, a digit and a
/// special character, or that holds the user's name, and so does this: it is
/// the one credential that opens everything, and `admin` is the first thing
/// anyone tries.
pub fn initial_admin_password() -> Result<Option<String>, String> {
    let Ok(password) = std::env::var(INITIAL_ADMIN_PASSWORD) else { return Ok(None) };
    if let Err(why) = initial_password_refusal(&password) {
        return Err(format!("{INITIAL_ADMIN_PASSWORD} failed validation: {why}"));
    }
    Ok(Some(password))
}

fn initial_password_refusal(password: &str) -> Result<(), &'static str> {
    if password.to_lowercase().contains("admin") {
        return Err("Password is similar to user name");
    }
    let strong = password.chars().count() >= 8
        && password.chars().any(|c| c.is_uppercase())
        && password.chars().any(|c| c.is_lowercase())
        && password.chars().any(|c| c.is_ascii_digit())
        && password.chars().any(|c| !c.is_alphanumeric());
    if !strong {
        return Err("Weak password. It needs at least 8 characters, with an uppercase letter, \
                    a lowercase letter, a digit and a special character");
    }
    Ok(())
}

/// The kinds a configuration is made of, as the cluster state carries them.
const KINDS: [&str; 6] =
    ["internalusers", "roles", "rolesmapping", "actiongroups", "tenants", "config"];

/// A configuration as the cluster state carries it: every document, and the
/// generation.
fn to_wire(cfg: &SecurityConfig) -> Value {
    let mut o = Map::new();
    for kind in KINDS {
        o.insert(kind.to_string(), cfg.document(kind));
    }
    o.insert("generation".into(), json!(cfg.generation));
    Value::Object(o)
}

fn from_wire(v: &Value) -> SecurityConfig {
    let docs: Vec<(&str, Value)> =
        KINDS.iter().filter_map(|k| v.get(*k).map(|d| (*k, d.clone()))).collect();
    let mut c = SecurityConfig::from_documents(&docs);
    c.generation = v.get("generation").and_then(|g| g.as_u64()).unwrap_or(0);
    c
}

impl Security {
    pub fn from_settings(settings: &Value) -> Arc<Security> {
        let get = |k: &str| crate::tls::node_setting(settings, k);
        let disabled = get("plugins.security.disabled").map(|v| v != "false").unwrap_or(true);
        let mut refusal = None;
        let config = if disabled { None } else { first_configuration(&mut refusal) };
        let configured = config.is_some();
        let config = config.unwrap_or_else(SecurityConfig::builtin);
        let restapi_roles = get("plugins.security.restapi.roles_enabled")
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().trim_matches(['[', ']', '"', '\'']).to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            // The reference defaults this to nothing at all: with no setting,
            // only an admin client certificate reaches the security API.
            // Defaulting it to two roles opened the API on a node whose
            // configuration never mentions it.
            .unwrap_or_default();
        // `<role>.<ENDPOINT>: [methods]`, read out of the settings tree
        // itself: a list does not survive being read as one string
        let endpoints_disabled = settings
            .pointer("/plugins/security/restapi/endpoints_disabled")
            .or_else(|| settings.pointer("/plugins.security.restapi.endpoints_disabled"))
            .and_then(|v| v.as_object())
            .map(|per_role| {
                per_role
                    .iter()
                    .map(|(role, kinds)| {
                        let kinds = kinds
                            .as_object()
                            .map(|o| {
                                o.iter()
                                    .map(|(kind, methods)| {
                                        let methods = methods
                                            .as_array()
                                            .map(|a| {
                                                a.iter()
                                                    .filter_map(|m| m.as_str())
                                                    .map(|m| m.to_ascii_uppercase())
                                                    .collect()
                                            })
                                            .unwrap_or_default();
                                        (kind.to_ascii_uppercase(), methods)
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        (role.clone(), kinds)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let allow_config_rewrite =
            get("plugins.security.unsupported.restapi.allow_securityconfig_modification")
                .map(|v| v == "true")
                .unwrap_or(false);
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
            audit: audit::AuditLog::new(settings, !disabled),
            config: RwLock::new(config),
            restapi_roles,
            endpoints_disabled,
            allow_config_rewrite,
            salt: get("plugins.security.compliance.salt")
                .unwrap_or_else(|| "e1ukloTsQlOgPquJ".into()),
            configured: std::sync::atomic::AtomicBool::new(configured),
            ready: std::sync::atomic::AtomicBool::new(configured),
            clustered: std::sync::atomic::AtomicBool::new(false),
            wire: parking_lot::Mutex::new(None),
            refusal,
        })
    }

    /// This node is one of a cluster: the configuration it read from its own
    /// files may be one the cluster has moved on from while it was away, so
    /// nobody is let in by it until the cluster manager's has been taken.
    pub fn join_cluster(&self) {
        use std::sync::atomic::Ordering::Release;
        self.clustered.store(true, Release);
        self.ready.store(false, Release);
    }

    /// Whether anybody may be let in here now, and if not, why.
    pub fn standing(&self) -> Standing {
        use std::sync::atomic::Ordering::Acquire;
        if !self.ready.load(Acquire) {
            return Standing::NotInitialized;
        }
        // A node that has lost its cluster manager -- cut off, or stopped
        // long enough to have been dropped -- cannot know what was revoked
        // while it was gone, and the moment it is back it may be following a
        // manager whose configuration it has not been sent yet.
        if self.clustered.load(Acquire) && !crate::cluster::has_manager() {
            return Standing::NoManager;
        }
        Standing::Ready
    }

    /// The configuration as the cluster state carries it; nothing where the
    /// node holds none.
    pub fn wire(&self) -> Option<Value> {
        use std::sync::atomic::Ordering::Acquire;
        if !self.enabled || !self.configured.load(Acquire) {
            return None;
        }
        let change = self.generation();
        if let Some((at, v)) = &*self.wire.lock()
            && *at == change
        {
            return Some(v.clone());
        }
        let v = to_wire(&self.config.read());
        *self.wire.lock() = Some((change, v.clone()));
        Some(v)
    }

    /// Take the configuration the cluster state carries, as the cluster
    /// manager (`leading`) or as a node following it.
    ///
    /// A follower takes whatever its manager published: the manager is the
    /// cluster's word, and a node whose own files say otherwise was away when
    /// they stopped being true. A manager keeps its own configuration unless
    /// the state holds a newer generation than it does -- it was elected over
    /// a state it had accepted and not yet applied -- or it holds none.
    pub fn settle(&self, published: Option<&Value>, leading: bool) {
        use std::sync::atomic::Ordering::{Acquire, Release};
        if !self.enabled || !self.clustered.load(Acquire) {
            return;
        }
        match published {
            Some(p) => {
                let newer = p.get("generation").and_then(|g| g.as_u64()).unwrap_or(0)
                    > self.config.read().generation;
                let take = if leading {
                    !self.configured.load(Acquire) || newer
                } else {
                    self.wire().as_ref() != Some(p)
                };
                if take {
                    self.take(p);
                }
            }
            // a cluster whose manager holds no configuration lets nobody in
            // through its other nodes either
            None if !leading => {
                self.ready.store(false, Release);
                return;
            }
            None => {}
        }
        self.ready.store(self.configured.load(Acquire), Release);
    }

    fn take(&self, published: &Value) {
        use std::sync::atomic::Ordering::Release;
        let next = from_wire(published);
        let mut cfg = self.config.write();
        // Saved as well as held, so the node restarts with it; a node that
        // cannot save it still answers by it, as the cluster's word, and takes
        // it again from the cluster when it comes back.
        if let Err(e) = next.save() {
            tracing::error!(
                "the security configuration from the cluster manager could not be saved in {}: {e}",
                security_dir().display()
            );
        }
        *cfg = next;
        self.configured.store(true, Release);
        self.touch(&cfg);
        drop(cfg);
        *self.wire.lock() = Some((self.generation(), published.clone()));
    }

    /// The cluster manager is gone: nobody is let in until there is one again
    /// and its configuration has been taken.
    pub fn lost_manager(&self) {
        use std::sync::atomic::Ordering::{Acquire, Release};
        if self.clustered.load(Acquire) {
            self.ready.store(false, Release);
        }
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
        if let Some(hit) = self.auth_cache.lock().get(&key)
            && hit.generation == generation
            && hit.at.elapsed() < self.cache_ttl
        {
            return Ok(hit.caller.clone());
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
    /// How many times the configuration has changed here.
    ///
    /// Anything that remembers an answer worked out under the rules has to
    /// forget it when the rules change, and this is what says they did.
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Acquire)
    }

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
        if let Some(dn) = &presented.peer_dn
            && self.admin_dns.iter().any(|a| *a == normalize_dn(dn))
        {
            let mut c = Caller::unrestricted();
            c.name = dn.clone();
            c.admin_cert = true;
            c.remote_address = presented.remote.clone();
            return Ok(c);
        }
        let key = presented_digest(presented);
        let generation = self.generation.load(std::sync::atomic::Ordering::Acquire);
        if let Some(hit) = self.auth_cache.lock().get(&key)
            && hit.generation == generation
            && hit.at.elapsed() < self.cache_ttl
        {
            return Ok(hit.caller.clone());
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

    /// The same, for one endpoint and one method.
    ///
    /// A role named in `endpoints_disabled` may not use the methods listed
    /// there, whatever `roles_enabled` says. Nothing read that setting, so
    /// an operator who had delegated read-only access to the security API had
    /// in fact delegated every method of it.
    pub fn may_administer_endpoint(&self, caller: &Caller, kind: &str, method: &str) -> bool {
        if !self.may_administer(caller) {
            return false;
        }
        if caller.unrestricted {
            return true;
        }
        let kind = kind.to_ascii_uppercase();
        let method = method.to_ascii_uppercase();
        let refused = |role: &String| {
            self.endpoints_disabled
                .get(role)
                .and_then(|kinds| kinds.get(&kind).or_else(|| kinds.get("*")))
                .map(|methods| methods.iter().any(|m| m == &method || m == "*"))
                .unwrap_or(false)
        };
        !caller.roles.iter().any(refused)
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
    // the two requests an on-behalf-of token does not authenticate are never
    // answered by a caller the same token authenticated for another request
    let token_refused = (p.method == "POST"
        && p.path.trim_end_matches('/').ends_with("/api/generateonbehalfoftoken"))
        || (p.method == "PUT" && p.path.trim_end_matches('/').ends_with("/api/account"));
    h.update([token_refused as u8]);
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
    // an alias's own filter narrows this index the same way, and for the same
    // reason: it is part of what the caller asked for rather than part of the
    // query they wrote
    let query = with_alias_filter(index, query);
    let Some(dls) = dls_for(store, index) else { return query };
    let base = query.unwrap_or_else(|| json!({"match_all": {}}));
    Some(json!({"bool": {"must": [base], "filter": [dls]}}))
}

/// The query as this index's alias filter has it.
pub fn with_alias_filter(index: &str, query: Option<Value>) -> Option<Value> {
    let Some(filter) = layer::alias_filter_for(index) else { return query };
    let base = query.unwrap_or_else(|| json!({"match_all": {}}));
    Some(json!({"bool": {"must": [base], "filter": [filter]}}))
}

/// Whether one document is inside the caller's view of its index.
pub fn doc_visible(store: &crate::store::Store, g: &crate::store::IdxState, id: &str) -> bool {
    let Some(dls) = dls_for(store, &g.name) else { return true };
    use velocore::collector::Count;
    use velocore::query::{BooleanQuery, Occur, TermQuery};
    use velocore::schema::IndexRecordOption;
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
        vectors: &g.vectors,
    };
    let Ok(filter) = crate::query::build(&ctx, &dls) else { return false };
    let probe =
        TermQuery::new(velocore::Term::from_field_text(g.fields.id, id), IndexRecordOption::Basic);
    let q = BooleanQuery::new(vec![
        (Occur::Must, Box::new(probe) as Box<dyn velocore::query::Query>),
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
        } else if view.masked(&name)
            && let Some(Value::Object(terms)) = o.get_mut(&name).and_then(|f| f.get_mut("terms"))
        {
            let raw: Vec<(String, Value)> =
                terms.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            terms.clear();
            for (k, v) in raw {
                terms.insert(view.mask_text(&k), v);
            }
        }
    }
}

/// Whether the caller may run every one of these actions over these
/// indices; the refusal names the whole list, as the plugin's shard-level
/// check does for a bulk, an mget or an msearch item.
/// Whether security is off on this node.
///
/// A caller carried in from another node says whether it was unrestricted
/// there; whether it is unrestricted *here* is this node's own business.
pub fn disabled_here() -> bool {
    audit::attached_store().map(|s| !s.security.enabled).unwrap_or(true)
}

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
    // An item names the indices it touches, and there is nothing here to
    // narrow it to: a partial grant is a refusal, not a pass. Letting
    // `Partial` through unnarrowed meant that with `do_not_fail_on_forbidden`
    // set, one item of a `_msearch` naming a granted index and a forbidden
    // one was answered from both -- the opposite of what the same verdict
    // does one layer up, where the request is cut down to what was granted.
    let denied =
        actions.iter().any(|a| !matches!(cfg.index_verdict(&caller, a, indices), Verdict::Allowed));
    if !denied {
        return None;
    }
    Some(format!("no permissions for [{}] and {}", actions.join(", "), caller.describe()))
}

/// The same, with a refusal written to the audit log the way the plugin
/// writes it for the transport request the item stands for: `actions[0]` is
/// that request's action, `named` the index expression the item gave.
pub fn item_refusal_audited(
    store: &crate::store::Store,
    actions: &[&str],
    named: &str,
    indices: &[String],
    body: impl FnOnce() -> Option<String>,
) -> Option<String> {
    let why = item_refusal(store, actions, indices)?;
    if let Some((audit, caller)) = audit_of() {
        let named: Vec<String> =
            named.split(',').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
        audit.missing_privileges_item(&caller, actions[0], &named, indices, body().as_deref());
    }
    Some(why)
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

// ---- the audit log, reached from the document paths ------------------------------

fn audit_of() -> Option<(Arc<audit::AuditLog>, Caller)> {
    let caller = layer::current_caller()?;
    let store = audit::attached_store()?;
    if !store.security.enabled {
        return None;
    }
    Some((store.security.audit.clone(), caller))
}

/// Whether any write is watched at all: a flag, before the caller is
/// even looked at, so an unwatched bulk costs nothing per document.
fn writes_watched_anywhere() -> bool {
    audit::attached_store()
        .map(|s| s.security.enabled && s.security.audit.any_write_watched())
        .unwrap_or(false)
}

fn reads_watched_anywhere() -> bool {
    audit::attached_store()
        .map(|s| s.security.enabled && s.security.audit.any_read_watched())
        .unwrap_or(false)
}

/// Whether writes to this index are watched for the current caller.
pub fn audit_watches_write(index: &str) -> bool {
    if !writes_watched_anywhere() {
        return false;
    }
    audit_of().map(|(a, c)| a.watches_write(index, &c.name)).unwrap_or(false)
}

/// A document written, for the compliance log.
pub fn audit_document_written(
    index: &str,
    id: &str,
    version: u64,
    before: Option<&Value>,
    after: Option<&Value>,
    deleted: bool,
) {
    if !writes_watched_anywhere() {
        return;
    }
    if let Some((a, c)) = audit_of() {
        a.document_written(&c, &c.remote_address, index, id, version, before, after, deleted);
    }
}

/// A document read, for the compliance log.
pub fn audit_document_read(index: &str, id: &str, source: &Value) {
    if !reads_watched_anywhere() {
        return;
    }
    if let Some((a, c)) = audit_of() {
        a.document_read(&c.name, index, id, source);
    }
}

/// Whether any read is watched at all, so search pages need not look.
pub fn audit_reads_watched(store: &crate::store::Store) -> bool {
    if !store.security.enabled {
        return false;
    }
    let cfg = store.security.audit.current();
    cfg.enabled && cfg.compliance.enabled && !cfg.compliance.read_watched_fields.is_empty()
}

/// An index-level event from inside a write: an index made for a first
/// document, a mapping grown by one.
pub fn audit_index_event(index: &str, action: &str, body: &str, with_headers: bool) {
    if let Some((a, c)) = audit_of() {
        a.index_event_inner(&c, action, index, body, with_headers);
    }
}

/// The mapping the plugin's auto-put carries: the properties just added.
pub fn mapping_added_body(raw: &Value, names: &[String]) -> String {
    let empty = serde_json::Map::new();
    let props = raw.get("properties").and_then(|p| p.as_object()).unwrap_or(&empty);
    let mut added = serde_json::Map::new();
    for n in names {
        if let Some(v) = props.get(n) {
            added.insert(n.clone(), v.clone());
        }
    }
    json!({"_doc": {"properties": added}}).to_string()
}

/// The mapping the plugin's auto-put carries: the properties that are new.
pub fn mapping_change_body(before: &Value, after: &Value) -> String {
    let empty = serde_json::Map::new();
    let b = before.get("properties").and_then(|p| p.as_object()).unwrap_or(&empty);
    let a = after.get("properties").and_then(|p| p.as_object()).unwrap_or(&empty);
    let mut props = serde_json::Map::new();
    for (k, v) in a {
        if b.get(k) != Some(v) {
            props.insert(k.clone(), v.clone());
        }
    }
    json!({"_doc": {"properties": props}}).to_string()
}

#[cfg(test)]
mod restapi_tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_role_may_be_barred_from_one_method_of_one_endpoint() {
        let settings = json!({
            "plugins": {
                "security": {
                    "disabled": "false",
                    "restapi": {
                        "roles_enabled": "readers",
                        "endpoints_disabled": {
                            "readers": {"internalusers": ["PUT", "DELETE"], "audit": ["*"]}
                        }
                    }
                }
            }
        });
        let security = Security::from_settings(&settings);
        let caller = Caller { roles: vec!["readers".into()], ..Caller::default() };
        assert!(security.may_administer(&caller));
        assert!(security.may_administer_endpoint(&caller, "INTERNALUSERS", "GET"));
        assert!(!security.may_administer_endpoint(&caller, "INTERNALUSERS", "PUT"));
        assert!(!security.may_administer_endpoint(&caller, "AUDIT", "GET"));
        // an admin certificate is not delegated access and is not narrowed
        assert!(security.may_administer_endpoint(&Caller::unrestricted(), "AUDIT", "GET"));
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;

    fn fresh_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("velo-secsave-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn with_user(generation: u64, name: &str) -> SecurityConfig {
        let mut c = SecurityConfig::builtin();
        c.users.insert(name.into(), InternalUser::from_json(&json!({"hash": "$2y$12$x"})));
        c.generation = generation;
        c
    }

    #[test]
    fn nothing_configured_is_no_configuration_and_no_demo_user() {
        let dir = fresh_dir("empty");
        assert!(SecurityConfig::load_from(&dir).unwrap().is_none());
        let builtin = SecurityConfig::builtin();
        assert!(builtin.users.is_empty(), "the built-in configuration holds no user");
        assert!(builtin.roles.contains_key("all_access"));
        let seeded = SecurityConfig::seeded("Seed-Password-1");
        assert_eq!(seeded.users.keys().collect::<Vec<_>>(), vec!["admin"]);
        assert!(seeded.authenticate("admin", "Seed-Password-1").is_some());
        assert!(seeded.authenticate("admin", "admin").is_none());
    }

    #[test]
    fn a_saved_generation_reads_back_whole() {
        let dir = fresh_dir("roundtrip");
        with_user(7, "alice").save_in(&dir).unwrap();
        let back = SecurityConfig::load_from(&dir).unwrap().unwrap();
        assert_eq!(back.generation, 7);
        assert!(back.users.contains_key("alice"));
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp") || n == PENDING)
            .collect();
        assert!(left.is_empty(), "left behind: {left:?}");
    }

    #[test]
    fn a_save_the_directory_refuses_is_an_error_and_changes_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = fresh_dir("refused");
        with_user(1, "alice").save_in(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let second = with_user(2, "bob").save_in(&dir);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        // root writes where it likes, and has nothing to show here
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        assert!(second.is_err(), "a save into a read-only directory was answered as done");
        let back = SecurityConfig::load_from(&dir).unwrap().unwrap();
        assert_eq!(back.generation, 1);
        assert!(back.users.contains_key("alice") && !back.users.contains_key("bob"));
    }

    #[test]
    fn a_save_stopped_before_it_was_decided_leaves_the_old_generation() {
        let dir = fresh_dir("undecided");
        with_user(1, "alice").save_in(&dir).unwrap();
        // every file of the next generation written, and the process stopped
        // before the marker
        with_user(2, "bob").write_undecided(&dir).unwrap();
        let back = SecurityConfig::load_from(&dir).unwrap().unwrap();
        assert_eq!(back.generation, 1);
        assert!(back.users.contains_key("alice") && !back.users.contains_key("bob"));
        assert!(!tmp_path(&dir, "internal_users.yml").exists());
    }

    #[test]
    fn a_save_stopped_between_its_files_is_finished_by_the_next_load() {
        let dir = fresh_dir("between");
        with_user(1, "alice").save_in(&dir).unwrap();
        with_user(2, "bob").write_undecided(&dir).unwrap();
        std::fs::write(dir.join(PENDING), b"").unwrap();
        // the users file renamed into place, the rest not yet
        std::fs::rename(tmp_path(&dir, "internal_users.yml"), dir.join("internal_users.yml"))
            .unwrap();
        let back = SecurityConfig::load_from(&dir).unwrap().unwrap();
        assert_eq!(back.generation, 2);
        assert!(back.users.contains_key("bob") && !back.users.contains_key("alice"));
        assert!(!dir.join(PENDING).exists());
    }

    #[test]
    fn a_configuration_that_cannot_be_read_whole_is_not_one() {
        let dir = fresh_dir("broken");
        with_user(1, "alice").save_in(&dir).unwrap();
        std::fs::remove_file(dir.join("roles_mapping.yml")).unwrap();
        assert!(SecurityConfig::load_from(&dir).is_err(), "a missing file was read past");
        with_user(1, "alice").save_in(&dir).unwrap();
        std::fs::write(dir.join("roles_mapping.yml"), ": : [ not yaml\n").unwrap();
        assert!(SecurityConfig::load_from(&dir).is_err(), "a file that is not YAML was read past");
    }

    #[test]
    fn the_initial_admin_password_must_be_strong() {
        for weak in [
            "admin",
            "password",
            "Password1",
            "Pass-1",
            "ALLUPPER-123",
            "lower-case-1",
            "My-Admin-Password-1",
        ] {
            assert!(initial_password_refusal(weak).is_err(), "{weak} was taken");
        }
        assert!(initial_password_refusal("Velo-Search-2026").is_ok());
    }
}
