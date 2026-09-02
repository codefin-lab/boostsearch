//! `_plugins/_security/api/*` and `_plugins/_security/authinfo`: the
//! plugin's REST API, answered in its words.

use axum::Extension;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use super::layer::unauthorized;
use super::{
    ActionGroup, Caller, InternalUser, Role, RoleMapping, SecurityConfig, Tenant, hash_password,
};
use crate::store::Store;

fn reply(status: StatusCode, word: &str, message: impl Into<String>) -> Response {
    (status, axum::Json(json!({"status": word, "message": message.into()}))).into_response()
}

fn not_found(kind: &str, name: &str) -> Response {
    reply(StatusCode::NOT_FOUND, "NOT_FOUND", format!("{kind} '{name}' not found."))
}

fn bad_request(message: impl Into<String>) -> Response {
    reply(StatusCode::BAD_REQUEST, "BAD_REQUEST", message)
}

fn created(name: &str) -> Response {
    reply(StatusCode::CREATED, "CREATED", format!("'{name}' created."))
}

fn updated(name: &str) -> Response {
    reply(StatusCode::OK, "OK", format!("'{name}' updated."))
}

fn deleted(name: &str) -> Response {
    reply(StatusCode::OK, "OK", format!("'{name}' deleted."))
}

fn ok_json(v: Value) -> Response {
    (StatusCode::OK, axum::Json(v)).into_response()
}

/// The plugin's "not allowed" answer for the API itself.
fn api_forbidden(caller: &Caller) -> Response {
    reply(
        StatusCode::FORBIDDEN,
        "FORBIDDEN",
        format!(
            "No permission to access REST API: User {} with Security roles [{}] does not have any role privileged for admin access. No client TLS certificate found in request",
            caller.name,
            caller.roles.join(", ")
        ),
    )
}

/// Who is asking, or why they may not be answered.
fn admin(store: &Store, caller: &Caller) -> Result<(), Response> {
    if !store.security.enabled {
        return Err(disabled());
    }
    if store.security.may_administer(caller) { Ok(()) } else { Err(api_forbidden(caller)) }
}

fn disabled() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(json!({"status": "SERVICE_UNAVAILABLE", "message": "OpenSearch Security not initialized."})),
    )
        .into_response()
}

fn parse(body: &str) -> Result<Value, Response> {
    let v: Value = serde_json::from_str(body)
        .map_err(|_| bad_request("Could not parse content of request."))?;
    if !v.is_object() {
        return Err(bad_request("Could not parse content of request."));
    }
    Ok(v)
}

/// Keys the plugin rejects in a body.
fn reject_unknown(kind: &str, body: &Value) -> Result<(), Response> {
    let allowed: &[&str] = match kind {
        "internalusers" => &[
            "hash",
            "password",
            "backend_roles",
            "attributes",
            "description",
            "opendistro_security_roles",
        ],
        "roles" => {
            &["cluster_permissions", "index_permissions", "tenant_permissions", "description"]
        }
        "rolesmapping" => &["backend_roles", "and_backend_roles", "hosts", "users", "description"],
        "actiongroups" => &["allowed_actions", "description", "type"],
        "tenants" => &["description"],
        _ => return Ok(()),
    };
    let mut wrong = Vec::new();
    if let Some(o) = body.as_object() {
        for k in o.keys() {
            if !allowed.contains(&k.as_str()) && k != "reserved" && k != "hidden" && k != "static" {
                wrong.push(k.clone());
            }
        }
    }
    if !wrong.is_empty() {
        let mut o = Map::new();
        o.insert("status".into(), json!("error"));
        o.insert("reason".into(), json!("Invalid configuration"));
        let mut inv = Map::new();
        inv.insert("keys".into(), json!(wrong.join(",")));
        o.insert("invalid_keys".into(), Value::Object(inv));
        return Err((StatusCode::BAD_REQUEST, axum::Json(Value::Object(o))).into_response());
    }
    Ok(())
}

// ---- the immutable checks -------------------------------------------------------

enum Entity<'a> {
    User(&'a InternalUser),
    Role(&'a Role),
    Mapping(&'a RoleMapping),
    Group(&'a ActionGroup),
    Tenant(&'a Tenant),
}

fn flags(e: &Entity) -> (bool, bool, bool) {
    match e {
        Entity::User(u) => (u.is_static, u.reserved, u.hidden),
        Entity::Role(r) => (r.is_static, r.reserved, r.hidden),
        Entity::Mapping(m) => (false, m.reserved, m.hidden),
        Entity::Group(g) => (g.is_static, g.reserved, g.hidden),
        Entity::Tenant(t) => (t.is_static, t.reserved, t.hidden),
    }
}

/// Hidden things are not there; static and reserved ones may not change.
fn immutable(kind: &str, name: &str, e: Option<Entity>) -> Result<(), Response> {
    let Some(e) = e else { return Ok(()) };
    let (is_static, reserved, hidden) = flags(&e);
    if hidden {
        return Err(not_found(kind, name));
    }
    if is_static {
        return Err(reply(
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
            format!("Resource '{name}' is static."),
        ));
    }
    if reserved {
        return Err(reply(
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
            format!("Resource '{name}' is reserved."),
        ));
    }
    Ok(())
}

fn entity<'a>(cfg: &'a SecurityConfig, kind: &str, name: &str) -> Option<Entity<'a>> {
    match kind {
        "internalusers" => cfg.users.get(name).map(Entity::User),
        "roles" => cfg.roles.get(name).map(Entity::Role),
        "rolesmapping" => cfg.mappings.get(name).map(Entity::Mapping),
        "actiongroups" => cfg.action_groups.get(name).map(Entity::Group),
        "tenants" => cfg.tenants.get(name).map(Entity::Tenant),
        _ => None,
    }
}

fn label(kind: &str) -> &'static str {
    match kind {
        "internalusers" => "user",
        "roles" => "role",
        "rolesmapping" => "rolesmapping",
        "actiongroups" => "actiongroup",
        "tenants" => "tenant",
        _ => "resource",
    }
}

/// The API's view of every entry of a kind: hidden ones left out, hashes blank.
fn listing(cfg: &SecurityConfig, kind: &str) -> Value {
    let mut o = Map::new();
    match kind {
        "internalusers" => {
            for (n, u) in &cfg.users {
                if !u.hidden {
                    o.insert(n.clone(), u.to_json());
                }
            }
        }
        "roles" => {
            for (n, r) in &cfg.roles {
                if !r.hidden {
                    o.insert(n.clone(), r.to_json());
                }
            }
        }
        "rolesmapping" => {
            for (n, m) in &cfg.mappings {
                if !m.hidden {
                    o.insert(n.clone(), m.to_json());
                }
            }
        }
        "actiongroups" => {
            for (n, g) in &cfg.action_groups {
                if !g.hidden {
                    o.insert(n.clone(), g.to_json());
                }
            }
        }
        "tenants" => {
            for (n, t) in &cfg.tenants {
                if !t.hidden {
                    o.insert(n.clone(), t.to_json());
                }
            }
        }
        _ => {}
    }
    Value::Object(o)
}

fn one(cfg: &SecurityConfig, kind: &str, name: &str) -> Option<Value> {
    listing(cfg, kind).get(name).cloned()
}

/// Write one entry from its JSON; the body has been validated.
fn put_entry(
    cfg: &mut SecurityConfig,
    kind: &str,
    name: &str,
    body: &Value,
) -> Result<(), Response> {
    match kind {
        "internalusers" => {
            let mut u = InternalUser::from_json(body);
            if let Some(p) = body.get("password").and_then(|p| p.as_str()) {
                if let Err(r) = validate_password(name, p) {
                    return Err(r);
                }
                u.hash = hash_password(p);
            } else if u.hash.is_empty() {
                // an existing user keeps their hash when neither is given
                match cfg.users.get(name) {
                    Some(old) => u.hash = old.hash.clone(),
                    None => {
                        return Err(bad_request(
                            "Please specify either 'hash' or 'password' when creating a new internal user.",
                        ));
                    }
                }
            }
            for r in &u.security_roles {
                if !cfg.roles.contains_key(r) {
                    return Err(reply(
                        StatusCode::NOT_FOUND,
                        "NOT_FOUND",
                        format!("role '{r}' not found."),
                    ));
                }
            }
            cfg.users.insert(name.to_string(), u);
        }
        "roles" => {
            let r = Role::from_json(body);
            for ip in &r.index_permissions {
                if let Some(dls) = &ip.dls {
                    if serde_json::from_str::<Value>(dls).is_err() {
                        return Err(bad_request(format!("Invalid DLS query: {dls}")));
                    }
                }
            }
            cfg.roles.insert(name.to_string(), r);
        }
        "rolesmapping" => {
            if !cfg.roles.contains_key(name) {
                return Err(reply(
                    StatusCode::NOT_FOUND,
                    "NOT_FOUND",
                    format!("role '{name}' not found."),
                ));
            }
            cfg.mappings.insert(name.to_string(), RoleMapping::from_json(body));
        }
        "actiongroups" => {
            let g = ActionGroup::from_json(body);
            if g.allowed_actions.iter().any(|a| a == name) {
                return Err(bad_request(format!("{name} cannot be an allowed_action of itself")));
            }
            cfg.action_groups.insert(name.to_string(), g);
        }
        "tenants" => {
            cfg.tenants.insert(name.to_string(), Tenant::from_json(body));
        }
        _ => return Err(not_found(label(kind), name)),
    }
    cfg.merge_documents(&[]);
    Ok(())
}

fn remove_entry(cfg: &mut SecurityConfig, kind: &str, name: &str) -> bool {
    let gone = match kind {
        "internalusers" => cfg.users.remove(name).is_some(),
        "roles" => cfg.roles.remove(name).is_some(),
        "rolesmapping" => cfg.mappings.remove(name).is_some(),
        "actiongroups" => cfg.action_groups.remove(name).is_some(),
        "tenants" => cfg.tenants.remove(name).is_some(),
        _ => false,
    };
    cfg.merge_documents(&[]);
    gone
}

/// The plugin's password rules, in its words.
fn validate_password(name: &str, password: &str) -> Result<(), Response> {
    if password.is_empty() {
        return Err(bad_request("Password does not match minimum criteria"));
    }
    if password.len() > 100 {
        return Err(bad_request("Password does not match minimum criteria"));
    }
    if !name.is_empty() && password.to_lowercase().contains(&name.to_lowercase()) && name.len() >= 4
    {
        return Err(bad_request("Password is similar to user name"));
    }
    Ok(())
}

fn required_fields(kind: &str, body: &Value) -> Result<(), Response> {
    let needed: &[&str] = match kind {
        "actiongroups" => &["allowed_actions"],
        _ => &[],
    };
    let missing: Vec<&str> = needed.iter().copied().filter(|f| body.get(*f).is_none()).collect();
    if !missing.is_empty() {
        let body = json!({
            "status": "error",
            "reason": "Invalid configuration",
            "missing_mandatory_keys": {"keys": missing.join(",")},
        });
        return Err((StatusCode::BAD_REQUEST, axum::Json(body)).into_response());
    }
    Ok(())
}

// ---- the resource handlers -------------------------------------------------------

pub async fn list(
    State(store): State<Store>,
    Extension(caller): Extension<Caller>,
    Path(kind): Path<String>,
) -> Response {
    if let Err(r) = admin(&store, &caller) {
        return r;
    }
    let cfg = store.security.config.read();
    match kind.as_str() {
        "internalusers" | "roles" | "rolesmapping" | "actiongroups" | "tenants" => {
            ok_json(listing(&cfg, &kind))
        }
        "securityconfig" => ok_json(cfg.document("config")),
        "nodesdn" => ok_json(json!({})),
        "allowlist" | "whitelist" => ok_json(json!({"config": {"enabled": false, "requests": {}}})),
        "audit" => ok_json(
            json!({"_readonly": ["/config/audit/ignore_users"], "config": {"enabled": false}}),
        ),
        _ => (StatusCode::NOT_FOUND, "").into_response(),
    }
}

pub async fn get_one(
    State(store): State<Store>,
    Extension(caller): Extension<Caller>,
    Path((kind, name)): Path<(String, String)>,
) -> Response {
    if let Err(r) = admin(&store, &caller) {
        return r;
    }
    let cfg = store.security.config.read();
    match one(&cfg, &kind, &name) {
        Some(v) => {
            let mut o = Map::new();
            o.insert(name, v);
            ok_json(Value::Object(o))
        }
        None => not_found(label(&kind), &name),
    }
}

pub async fn put_one(
    State(store): State<Store>,
    Extension(caller): Extension<Caller>,
    Path((kind, name)): Path<(String, String)>,
    body: String,
) -> Response {
    if let Err(r) = admin(&store, &caller) {
        return r;
    }
    let body = match parse(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(r) = reject_unknown(&kind, &body) {
        return r;
    }
    if let Err(r) = required_fields(&kind, &body) {
        return r;
    }
    if kind == "internalusers" && body.get("hash").is_some() && body.get("password").is_some() {
        return bad_request(
            "Please specify either 'hash' or 'password' when creating a new internal user.",
        );
    }
    let mut cfg = store.security.config.write();
    if let Err(r) = immutable(label(&kind), &name, entity(&cfg, &kind, &name)) {
        return r;
    }
    let existed = one(&cfg, &kind, &name).is_some();
    if let Err(r) = put_entry(&mut cfg, &kind, &name, &body) {
        return r;
    }
    let _ = cfg.save();
    store.security.touch();
    if existed { updated(&name) } else { created(&name) }
}

pub async fn delete_one(
    State(store): State<Store>,
    Extension(caller): Extension<Caller>,
    Path((kind, name)): Path<(String, String)>,
) -> Response {
    if let Err(r) = admin(&store, &caller) {
        return r;
    }
    let mut cfg = store.security.config.write();
    if let Err(r) = immutable(label(&kind), &name, entity(&cfg, &kind, &name)) {
        return r;
    }
    if !remove_entry(&mut cfg, &kind, &name) {
        return not_found(label(&kind), &name);
    }
    let _ = cfg.save();
    store.security.touch();
    deleted(&name)
}

/// JSON Patch over one entry or over the whole kind.
fn apply_patch(target: &mut Value, ops: &Value) -> Result<(), String> {
    let Some(ops) = ops.as_array() else { return Err("Invalid patch".into()) };
    for op in ops {
        let kind = op.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let path = op.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let value = op.get("value").cloned();
        let parts: Vec<String> = path
            .trim_start_matches('/')
            .split('/')
            .map(|s| s.replace("~1", "/").replace("~0", "~"))
            .collect();
        if parts.is_empty() || parts[0].is_empty() {
            return Err("Invalid patch path".into());
        }
        let (last, parents) = parts.split_last().unwrap();
        let mut node = &mut *target;
        for p in parents {
            node = match node {
                Value::Object(o) => o.entry(p.clone()).or_insert(Value::Object(Map::new())),
                Value::Array(a) => {
                    let i: usize = p.parse().map_err(|_| "Invalid patch path".to_string())?;
                    a.get_mut(i).ok_or("Invalid patch path")?
                }
                _ => return Err("Invalid patch path".into()),
            };
        }
        match (kind, node) {
            ("add" | "replace", Value::Object(o)) => {
                o.insert(last.clone(), value.ok_or("Missing value")?);
            }
            ("add", Value::Array(a)) => {
                let v = value.ok_or("Missing value")?;
                if last == "-" {
                    a.push(v);
                } else {
                    let i: usize = last.parse().map_err(|_| "Invalid patch path".to_string())?;
                    if i > a.len() {
                        return Err("Invalid patch path".into());
                    }
                    a.insert(i, v);
                }
            }
            ("replace", Value::Array(a)) => {
                let i: usize = last.parse().map_err(|_| "Invalid patch path".to_string())?;
                *a.get_mut(i).ok_or("Invalid patch path")? = value.ok_or("Missing value")?;
            }
            ("remove", Value::Object(o)) => {
                o.remove(last);
            }
            ("remove", Value::Array(a)) => {
                let i: usize = last.parse().map_err(|_| "Invalid patch path".to_string())?;
                if i < a.len() {
                    a.remove(i);
                }
            }
            _ => return Err(format!("Unsupported patch op: {kind}")),
        }
    }
    Ok(())
}

pub async fn patch_one(
    State(store): State<Store>,
    Extension(caller): Extension<Caller>,
    Path((kind, name)): Path<(String, String)>,
    body: String,
) -> Response {
    if let Err(r) = admin(&store, &caller) {
        return r;
    }
    let ops: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return bad_request("Could not parse content of request."),
    };
    let mut cfg = store.security.config.write();
    if let Err(r) = immutable(label(&kind), &name, entity(&cfg, &kind, &name)) {
        return r;
    }
    let Some(mut current) = one(&cfg, &kind, &name) else { return not_found(label(&kind), &name) };
    if kind == "internalusers" {
        current["hash"] = json!(cfg.users.get(&name).map(|u| u.hash.clone()).unwrap_or_default());
    }
    if let Err(e) = apply_patch(&mut current, &ops) {
        return bad_request(e);
    }
    if let Some(o) = current.as_object_mut() {
        o.remove("reserved");
        o.remove("hidden");
        o.remove("static");
    }
    if let Err(r) = reject_unknown(&kind, &current) {
        return r;
    }
    if let Err(r) = put_entry(&mut cfg, &kind, &name, &current) {
        return r;
    }
    let _ = cfg.save();
    store.security.touch();
    reply(StatusCode::OK, "OK", format!("'{name}' updated."))
}

pub async fn patch_all(
    State(store): State<Store>,
    Extension(caller): Extension<Caller>,
    Path(kind): Path<String>,
    body: String,
) -> Response {
    if let Err(r) = admin(&store, &caller) {
        return r;
    }
    let ops: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return bad_request("Could not parse content of request."),
    };
    let mut cfg = store.security.config.write();
    if kind == "securityconfig" {
        let mut current = cfg.document("config");
        if let Err(e) = apply_patch(&mut current, &ops) {
            return bad_request(e);
        }
        cfg.dynamic = current.get("config").cloned().unwrap_or(Value::Object(Map::new()));
        let _ = cfg.save();
    store.security.touch();
        return reply(StatusCode::OK, "OK", "Resource updated.");
    }
    let mut current = listing(&cfg, &kind);
    if kind == "internalusers" {
        for (n, u) in &cfg.users {
            if let Some(v) = current.get_mut(n) {
                v["hash"] = json!(u.hash);
            }
        }
    }
    // every named entry must be free to change
    if let Some(a) = ops.as_array() {
        for op in a {
            let path = op.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let name = path.trim_start_matches('/').split('/').next().unwrap_or("");
            if let Err(r) = immutable(label(&kind), name, entity(&cfg, &kind, name)) {
                return r;
            }
        }
    }
    let before = current.clone();
    if let Err(e) = apply_patch(&mut current, &ops) {
        return bad_request(e);
    }
    let Some(after) = current.as_object() else { return bad_request("Invalid patch") };
    let before_o = before.as_object().cloned().unwrap_or_default();
    for (n, v) in after {
        if before_o.get(n) != Some(v) {
            let mut v = v.clone();
            if let Some(o) = v.as_object_mut() {
                o.remove("reserved");
                o.remove("hidden");
                o.remove("static");
            }
            if let Err(r) = reject_unknown(&kind, &v) {
                return r;
            }
            if let Err(r) = put_entry(&mut cfg, &kind, n, &v) {
                return r;
            }
        }
    }
    for n in before_o.keys() {
        if !after.contains_key(n) {
            remove_entry(&mut cfg, &kind, n);
        }
    }
    let _ = cfg.save();
    store.security.touch();
    reply(StatusCode::OK, "OK", "Resource updated.")
}

// ---- account, authinfo, certs -------------------------------------------------------

pub async fn account(State(store): State<Store>, Extension(caller): Extension<Caller>) -> Response {
    if !store.security.enabled {
        return disabled();
    }
    let cfg = store.security.config.read();
    let user = cfg.users.get(&caller.name);
    let tenants = tenants_of(&cfg, &caller);
    ok_json(json!({
        "user_name": caller.name,
        "is_reserved": user.map(|u| u.reserved).unwrap_or(false),
        "is_hidden": user.map(|u| u.hidden).unwrap_or(false),
        "is_internal_user": caller.is_internal,
        "user_requested_tenant": caller.requested_tenant,
        "backend_roles": caller.backend_roles,
        "custom_attribute_names": caller.attributes.keys().map(|k| format!("attr.internal.{k}")).collect::<Vec<_>>(),
        "tenants": tenants,
        "roles": caller.roles,
    }))
}

fn tenants_of(cfg: &SecurityConfig, caller: &Caller) -> Value {
    let mut o = Map::new();
    o.insert(caller.name.clone(), json!(true));
    for role in caller.roles.iter().filter_map(|r| cfg.roles.get(r)) {
        for tp in &role.tenant_permissions {
            let write = tp.allowed_actions.iter().any(|a| a == "kibana_all_write");
            for (name, _) in cfg.tenants.iter().filter(|(_, t)| !t.hidden) {
                if super::any_matches(&tp.tenant_patterns, name) {
                    let cur = o.get(name).and_then(|v| v.as_bool()).unwrap_or(false);
                    o.insert(name.clone(), json!(cur || write));
                }
            }
        }
    }
    if cfg.roles.iter().any(|(n, _)| n == "all_access" && caller.roles.contains(n)) {
        for (name, _) in cfg.tenants.iter().filter(|(_, t)| !t.hidden) {
            o.insert(name.clone(), json!(true));
        }
    }
    Value::Object(o)
}

pub async fn change_password(
    State(store): State<Store>,
    Extension(caller): Extension<Caller>,
    body: String,
) -> Response {
    if !store.security.enabled {
        return disabled();
    }
    let body = match parse(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(password) = body.get("password").and_then(|v| v.as_str()) else {
        return bad_request("Missing field \"password\"");
    };
    let Some(current) = body.get("current_password").and_then(|v| v.as_str()) else {
        return bad_request("Missing field \"current_password\"");
    };
    let mut cfg = store.security.config.write();
    if cfg.authenticate(&caller.name, current).is_none() {
        return bad_request("Could not validate your current password.");
    }
    if let Err(r) = validate_password(&caller.name, password) {
        return r;
    }
    if let Some(u) = cfg.users.get_mut(&caller.name) {
        u.hash = hash_password(password);
    }
    let _ = cfg.save();
    store.security.touch();
    reply(StatusCode::OK, "OK", format!("'{}' updated.", caller.name))
}

pub async fn authinfo(
    State(store): State<Store>,
    Extension(caller): Extension<Caller>,
) -> Response {
    if !store.security.enabled {
        return disabled();
    }
    let cfg = store.security.config.read();
    let mut attrs = Map::new();
    for (k, v) in &caller.attributes {
        attrs.insert(format!("attr.internal.{k}"), json!(v));
    }
    ok_json(json!({
        "user": caller.describe(),
        "user_name": caller.name,
        "user_requested_tenant": caller.requested_tenant,
        "remote_address": if caller.remote_address.is_empty() { Value::Null } else { json!(format!("{}:0", caller.remote_address)) },
        "backend_roles": caller.backend_roles,
        "custom_attribute_names": attrs.keys().cloned().collect::<Vec<_>>(),
        "roles": caller.roles,
        "tenants": tenants_of(&cfg, &caller),
        "principal": Value::Null,
        "peer_certificates": "0",
        "sso_logout_url": Value::Null,
    }))
}

pub async fn health() -> Response {
    ok_json(json!({"message": Value::Null, "mode": "strict", "status": "UP"}))
}

pub async fn whoami(State(store): State<Store>, Extension(caller): Extension<Caller>) -> Response {
    if !store.security.enabled {
        return disabled();
    }
    ok_json(
        json!({"dn": Value::Null, "is_admin": store.security.may_administer(&caller), "is_node_certificate_request": false}),
    )
}

pub async fn permissions_info(
    State(store): State<Store>,
    Extension(caller): Extension<Caller>,
) -> Response {
    if !store.security.enabled {
        return disabled();
    }
    let allowed = store.security.may_administer(&caller);
    ok_json(json!({
        "user": caller.describe(),
        "user_name": caller.name,
        "has_api_access": allowed,
        "disabled_endpoints": {},
    }))
}

/// The node's own certificates, as `ssl/certs` describes them.
pub async fn certs(State(store): State<Store>, Extension(caller): Extension<Caller>) -> Response {
    if let Err(r) = admin(&store, &caller) {
        return r;
    }
    // the plugin hands certificates only to an admin certificate, never to
    // a password; `plugins.security.ssl_cert_reload_enabled` aside, a basic
    // caller is refused
    if !caller.admin_cert {
        return reply(StatusCode::FORBIDDEN, "FORBIDDEN", "Access denied");
    }
    let settings = crate::tls::node_settings();
    let tls = crate::tls::TlsSettings::read(&settings);
    let list = match tls.cert.as_ref().and_then(|c| std::fs::read(c).ok()) {
        Some(pem) => describe_certs(&pem),
        None => Vec::new(),
    };
    ok_json(json!({"http_certificates_list": list, "transport_certificates_list": list}))
}

fn describe_certs(pem: &[u8]) -> Vec<Value> {
    let mut out = Vec::new();
    for item in x509_parser::pem::Pem::iter_from_buffer(pem).flatten() {
        let Ok(cert) = item.parse_x509() else { continue };
        let san: Vec<String> = cert
            .subject_alternative_name()
            .ok()
            .flatten()
            .map(|s| s.value.general_names.iter().map(|g| format!("{g}")).collect())
            .unwrap_or_default();
        // ASN1Time prints as `Jan  1 00:00:00 2026 +00:00`; the plugin prints
        // RFC 3339, so the pieces are laid out again
        let fmt = |t: x509_parser::time::ASN1Time| rfc3339(t.timestamp());
        out.push(json!({
            "issuer_dn": cert.issuer().to_string(),
            "subject_dn": cert.subject().to_string(),
            "san": format!("[{}]", san.join(", ")),
            "not_before": fmt(cert.validity().not_before),
            "not_after": fmt(cert.validity().not_after),
        }));
    }
    out
}

/// A Unix timestamp as `2026-01-01T00:00:00.000Z`.
fn rfc3339(ts: i64) -> String {
    // civil date from days since the epoch (Howard Hinnant's algorithm)
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
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Anything under the prefix nothing answers.
pub async fn unknown(State(store): State<Store>, Extension(caller): Extension<Caller>) -> Response {
    if !store.security.enabled {
        return disabled();
    }
    if !store.security.may_administer(&caller) {
        return api_forbidden(&caller);
    }
    (StatusCode::NOT_FOUND, "").into_response()
}

pub fn _unused() -> Response {
    unauthorized()
}
