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

/// What the plugin answers a request it will not act on at all: a `reason`
/// rather than a `message`, and the word `error` rather than the status. Its
/// own refusals of a missing entity keep the other shape, which is why both
/// live here.
fn invalid(reason: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, axum::Json(json!({"status": "error", "reason": reason.into()})))
        .into_response()
}

/// The same, naming the field whose type was wrong.
fn wrong_datatype(field: &str, expected: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({"status": "error", "reason": "Wrong datatype", field: expected})),
    )
        .into_response()
}

/// The fields each kind keeps a list in.
///
/// A caller who writes one as something else was taken at their word and the
/// entity written; the reference answers which field it was and what it
/// expected, and a role whose `index_permissions` is a string grants nothing
/// while looking as though it grants something.
fn array_fields(kind: &str, body: &Value) -> Result<(), Response> {
    let listed: &[&str] = match kind {
        "roles" => &["index_permissions", "cluster_permissions", "tenant_permissions"],
        "rolesmapping" => &["users", "backend_roles", "hosts", "and_backend_roles"],
        "internalusers" => &["backend_roles", "opendistro_security_roles"],
        "actiongroups" => &["allowed_actions"],
        _ => &[],
    };
    for field in listed {
        if let Some(v) = body.get(*field)
            && !v.is_array()
        {
            return Err(wrong_datatype(field, "Array expected"));
        }
    }
    Ok(())
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

/// The header a change's answer carries its generation in, from the cluster
/// manager that made it to the node the caller asked, which holds the answer
/// until it has taken that generation itself and takes the header off.
pub const GENERATION_HEADER: &str = "x-velosearch-security-generation";

/// The plugin's answer to a change it could not make: a 500 saying why.
fn not_saved(why: impl std::fmt::Display) -> Response {
    reply(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_SERVER_ERROR", format!("Error {why}"))
}

fn no_cluster_manager() -> Response {
    crate::api::err(
        StatusCode::SERVICE_UNAVAILABLE,
        "cluster_block_exception",
        "blocked by: [SERVICE_UNAVAILABLE/2/no cluster-manager];",
    )
}

/// Whether the configuration may be changed here.
///
/// It is the cluster's, and the cluster manager keeps it: a change is sent to
/// the manager, and a node that has none -- or has just stopped being it --
/// refuses the change as OpenSearch's `no cluster-manager` block refuses a
/// write to the security index.
fn writable_here() -> Result<(), Response> {
    if crate::cluster::has_manager() && crate::cluster::is_cluster_manager() {
        Ok(())
    } else {
        Err(no_cluster_manager())
    }
}

/// Save the next configuration and put it in force in place of the one held,
/// under the lock that numbers the changes; or answer why it was not.
///
/// The save's error used to be thrown away, and the change answered as made:
/// it was in force in memory until the node restarted, and then it was gone.
/// Nothing is put in force that is not on disk first.
fn install(
    store: &Store,
    cfg: &mut SecurityConfig,
    mut next: SecurityConfig,
) -> Result<u64, Response> {
    next.generation = cfg.generation + 1;
    if let Err(e) = next.save() {
        let dir = super::security_dir();
        tracing::error!("the security configuration could not be saved in {}: {e}", dir.display());
        return Err(not_saved(format!(
            "the security configuration could not be saved in {}: {e}",
            dir.display()
        )));
    }
    *cfg = next;
    store.security.touch(cfg);
    Ok(cfg.generation)
}

/// Hold the answer until the cluster has committed the change: until then it
/// is this node's alone, and a node that lost the cluster manager's seat in
/// the meantime has made a change the cluster will not keep.
async fn published(generation: u64) -> Result<(), Response> {
    let Some(rt) = crate::cluster::runtime() else { return Ok(()) };
    rt.republish();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let there = rt.with_state(|s| {
            s.customs
                .pointer("/security/generation")
                .and_then(|g| g.as_u64())
                .is_some_and(|g| g >= generation)
        });
        if there {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(if crate::cluster::has_manager() {
                not_saved(
                    "the change was saved on the cluster manager but the cluster did not commit it in time",
                )
            } else {
                no_cluster_manager()
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// A change's answer, carrying the generation it made.
fn at_generation(mut r: Response, generation: u64) -> Response {
    if let Ok(v) = axum::http::HeaderValue::from_str(&generation.to_string()) {
        r.headers_mut().insert(GENERATION_HEADER, v);
    }
    r
}

/// The name the on-behalf-of token route is judged under, as a cluster
/// permission.
pub const OBO_ACTION: &str = "security:obo/create";

/// Why the API refused a caller, carried on the refusal for the security
/// layer to write down: the plugin audits the refusal with its reason as the
/// privilege, and only the layer holds the request the record quotes.
#[derive(Clone, Debug)]
pub struct ApiRefused(pub String);

/// The plugin's "not allowed" answer for the API itself.
fn api_forbidden(caller: &Caller) -> Response {
    let why = format!(
        "User {} with Security roles [{}] does not have any role privileged for admin access. No client TLS certificate found in request",
        caller.name,
        caller.roles.join(", ")
    );
    let mut r = reply(
        StatusCode::FORBIDDEN,
        "FORBIDDEN",
        format!("No permission to access REST API: {why}"),
    );
    r.extensions_mut().insert(ApiRefused(why));
    r
}

/// Who is asking, or why they may not be answered.
fn admin(store: &Store, caller: &Caller) -> Result<(), Response> {
    if !store.security.enabled {
        return Err(disabled());
    }
    if store.security.may_administer(caller) { Ok(()) } else { Err(api_forbidden(caller)) }
}

/// The same, for one endpoint of the API and one method of it.
///
/// `plugins.security.restapi.endpoints_disabled.<role>.<ENDPOINT>` names the
/// methods a delegated role may not use. Nothing read it, so a role given
/// read-only access to the API in fact had every method of it.
fn admin_for(store: &Store, caller: &Caller, kind: &str, method: &str) -> Result<(), Response> {
    admin(store, caller)?;
    if store.security.may_administer_endpoint(caller, &endpoint_name(kind), method) {
        Ok(())
    } else {
        Err(api_forbidden(caller))
    }
}

/// The name an endpoint goes by in `endpoints_disabled`, which is not always
/// the word in the path: `securityconfig` is configured as `CONFIG`.
fn endpoint_name(kind: &str) -> String {
    match kind.trim_matches('/').to_ascii_uppercase().as_str() {
        "SECURITYCONFIG" => "CONFIG".to_string(),
        other => other.to_string(),
    }
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
                validate_password(name, p)?;
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
                if let Some(dls) = &ip.dls
                    && serde_json::from_str::<Value>(dls).is_err()
                {
                    return Err(bad_request(format!("Invalid DLS query: {dls}")));
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

/// The plugin's password rules, in its words, measured against OpenSearch
/// 3.1.0 rather than read off its settings.
///
/// Three rules answer in the reference's own envelope. A password of more
/// than a hundred characters is `Password is too long`. A password holding
/// the user's name, where that name is four characters or more, is `Password
/// is similar to user name` -- `deer` is refused and `dee` is not, which is
/// where the four comes from. Anything the reference thinks weak is `Weak
/// password`, and that judgement is a strength estimate rather than a rule:
/// it refuses `abcdefghij`, `Dee123456x` and a hundred characters of `Aa1`
/// followed by `x`, and accepts `Abcdefgh1`, `dee-password-1` and
/// `Zq7-mesa-lantern-42`. Nothing shorter than nine characters was accepted
/// by it in any shape, so nine is the floor kept here; the estimate itself
/// is not reproduced, and what it refuses above nine is accepted here.
fn validate_password(name: &str, password: &str) -> Result<(), Response> {
    if password.chars().count() > 100 {
        return Err(invalid("Password is too long"));
    }
    if password.chars().count() < 9 {
        return Err(invalid("Weak password"));
    }
    if !name.is_empty() && password.to_lowercase().contains(&name.to_lowercase()) && name.len() >= 4
    {
        return Err(invalid("Password is similar to user name"));
    }
    Ok(())
}

/// Every password a patch of internal users sets, checked and hashed now and
/// written into the patch as the hash, so that no lock is held while bcrypt
/// works (see `put_one`). `user` is the user a patch of one entry names; a
/// patch of them all names each in the first part of its path.
fn hash_patched_passwords(ops: &mut Value, user: Option<&str>) -> Result<(), Response> {
    let Some(ops) = ops.as_array_mut() else { return Ok(()) };
    for op in ops {
        if op.get("op").and_then(|v| v.as_str()) == Some("remove") {
            continue;
        }
        let path = op.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let parts: Vec<String> = path
            .trim_start_matches('/')
            .split('/')
            .map(|s| s.replace("~1", "/").replace("~0", "~"))
            .collect();
        let (owner, rest) = match user {
            Some(u) => (u.to_string(), &parts[..]),
            None => (parts[0].clone(), &parts[1..]),
        };
        if rest.len() == 1 && rest[0] == "password" {
            let Some(password) = op.get("value").and_then(|v| v.as_str()).map(str::to_string)
            else {
                continue;
            };
            validate_password(&owner, &password)?;
            op["path"] = json!(format!("{}hash", &path[..path.len() - "password".len()]));
            op["value"] = json!(hash_password(&password));
        } else if rest.is_empty()
            && let Some(password) =
                op.pointer("/value/password").and_then(|v| v.as_str()).map(str::to_string)
        {
            validate_password(&owner, &password)?;
            if let Some(o) = op.get_mut("value").and_then(|v| v.as_object_mut()) {
                o.remove("password");
                o.insert("hash".into(), json!(hash_password(&password)));
            }
        }
    }
    Ok(())
}

/// Whether a patch writes both a password and a hash, which is the one thing
/// `PUT` refuses about a user and `PATCH` did not.
fn sets_both_password_and_hash(ops: &Value) -> bool {
    let touches = |what: &str| {
        ops.as_array()
            .map(|a| {
                a.iter().any(|op| {
                    op.get("op").and_then(|v| v.as_str()) != Some("remove")
                        && op
                            .get("path")
                            .and_then(|v| v.as_str())
                            .map(|p| p.trim_end_matches('/').ends_with(what))
                            .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    };
    touches("password") && touches("hash")
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
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    if let Err(r) = admin_for(&store, &caller, &kind, "GET") {
        return r;
    }
    let cfg = store.security.config.read();
    match kind.as_str() {
        "internalusers" => {
            // `filterBy=service` lists the service accounts, `internal` the
            // rest; anything else, or nothing, lists everyone
            let filter =
                query.as_deref().unwrap_or("").split('&').find_map(|pair| {
                    pair.strip_prefix("filterBy=").map(|v| v.to_ascii_lowercase())
                });
            let mut all = listing(&cfg, &kind);
            if let (Some(filter), Some(o)) = (filter.as_deref(), all.as_object_mut()) {
                let wanted = match filter {
                    "service" => Some(true),
                    "internal" => Some(false),
                    _ => None,
                };
                if let Some(wanted) = wanted {
                    o.retain(|name, _| {
                        cfg.users.get(name).map(is_service_account).unwrap_or(false) == wanted
                    });
                }
            }
            ok_json(all)
        }
        "roles" | "rolesmapping" | "actiongroups" | "tenants" => ok_json(listing(&cfg, &kind)),
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
    if let Err(r) = admin_for(&store, &caller, &kind, "GET") {
        return r;
    }
    let cfg = store.security.config.read();
    match one(&cfg, &kind, &name) {
        Some(v) => {
            let mut o = Map::new();
            o.insert(name.clone(), v);
            let entry = Value::Object(o);
            let whole = cfg.document(&kind);
            store.security.audit.internal_config_read_with(
                &caller,
                &caller.remote_address,
                &kind,
                Some(&whole),
            );
            ok_json(entry)
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
    if let Err(r) = admin_for(&store, &caller, &kind, "PUT") {
        return r;
    }
    let mut body = match parse(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    // These three are the plugin's own bookkeeping, not a caller's to set --
    // the patch paths already strip them and this one did not. A role written
    // with `hidden: true` is granted like any other and is left out of every
    // listing, and `DELETE` answers that it is not there: an administrator
    // could give themselves an invisible role that nobody can find or remove.
    if let Some(o) = body.as_object_mut() {
        o.remove("reserved");
        o.remove("hidden");
        o.remove("static");
    }
    // An empty document is not one to write, and the reference says so before
    // it looks at what the name refers to: a mapping to nobody was answered
    // here with the absence of the role it named instead.
    if body.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        return invalid("Request body required for this action.");
    }
    if let Err(r) = array_fields(&kind, &body) {
        return r;
    }
    if let Err(r) = reject_unknown(&kind, &body) {
        return r;
    }
    if let Err(r) = required_fields(&kind, &body) {
        return r;
    }
    // A service account is given no secret by whoever creates it: it is
    // refused one, and gets a random one nobody knows until a token is asked
    // for. The hash is made before the configuration is locked, as bcrypt is
    // slow by design.
    if kind == "internalusers"
        && body
            .pointer("/attributes/service")
            .and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| Some(v.to_string())))
            .map(|s| s.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    {
        let given = |field: &str| {
            body.get(field).and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false)
        };
        if given("password") {
            return bad_request(format!(
                "A password cannot be provided for a service account. Failed to register service account: {name}"
            ));
        }
        if given("hash") {
            return bad_request(format!(
                "A password hash cannot be provided for service account. Failed to register service account: {name}"
            ));
        }
        body["hash"] = json!(hash_password(&service_password()));
    }
    // A password is hashed here too, before the lock rather than under it.
    // Every request on the node reads the configuration, the cluster's own
    // checks among them: hashing under the write lock held them all for the
    // length of a bcrypt, and a cluster manager taking changes one after
    // another answered its followers late enough to be voted out.
    if kind == "internalusers"
        && let Some(password) = body.get("password").and_then(|p| p.as_str()).map(str::to_string)
    {
        if let Err(r) = validate_password(&name, &password) {
            return r;
        }
        body["hash"] = json!(hash_password(&password));
        if let Some(o) = body.as_object_mut() {
            o.remove("password");
        }
    }
    if let Err(r) = writable_here() {
        return r;
    }
    let (answer, generation) = {
        let mut cfg = store.security.config.write();
        if let Err(r) = immutable(label(&kind), &name, entity(&cfg, &kind, &name)) {
            return r;
        }
        let existed = one(&cfg, &kind, &name).is_some();
        let before = cfg.document(&kind);
        let mut next = cfg.clone();
        if let Err(r) = put_entry(&mut next, &kind, &name, &body) {
            return r;
        }
        let generation = match install(&store, &mut cfg, next) {
            Ok(g) => g,
            Err(r) => return r,
        };
        let after = cfg.document(&kind);
        store.security.audit.internal_config_written_with(
            &caller,
            &caller.remote_address,
            &kind,
            Some(&before),
            Some(&after),
        );
        (if existed { updated(&name) } else { created(&name) }, generation)
    };
    if let Err(r) = published(generation).await {
        return r;
    }
    at_generation(answer, generation)
}

pub async fn delete_one(
    State(store): State<Store>,
    Extension(caller): Extension<Caller>,
    Path((kind, name)): Path<(String, String)>,
) -> Response {
    if let Err(r) = admin_for(&store, &caller, &kind, "DELETE") {
        return r;
    }
    if let Err(r) = writable_here() {
        return r;
    }
    let generation = {
        let mut cfg = store.security.config.write();
        if let Err(r) = immutable(label(&kind), &name, entity(&cfg, &kind, &name)) {
            return r;
        }
        let before = cfg.document(&kind);
        let mut next = cfg.clone();
        if !remove_entry(&mut next, &kind, &name) {
            return not_found(label(&kind), &name);
        }
        let generation = match install(&store, &mut cfg, next) {
            Ok(g) => g,
            Err(r) => return r,
        };
        let after = cfg.document(&kind);
        store.security.audit.internal_config_written_with(
            &caller,
            &caller.remote_address,
            &kind,
            Some(&before),
            Some(&after),
        );
        generation
    };
    if let Err(r) = published(generation).await {
        return r;
    }
    at_generation(deleted(&name), generation)
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
    if let Err(r) = admin_for(&store, &caller, &kind, "PATCH") {
        return r;
    }
    let mut ops: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return bad_request("Could not parse content of request."),
    };
    let both = kind == "internalusers" && sets_both_password_and_hash(&ops);
    if kind == "internalusers"
        && !both
        && let Err(r) = hash_patched_passwords(&mut ops, Some(&name))
    {
        return r;
    }
    if let Err(r) = writable_here() {
        return r;
    }
    let generation = {
        let mut cfg = store.security.config.write();
        if let Err(r) = immutable(label(&kind), &name, entity(&cfg, &kind, &name)) {
            return r;
        }
        let Some(mut current) = one(&cfg, &kind, &name) else {
            return not_found(label(&kind), &name);
        };
        if kind == "internalusers" {
            current["hash"] =
                json!(cfg.users.get(&name).map(|u| u.hash.clone()).unwrap_or_default());
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
        // the checks `PUT` makes, which this path skipped: an action group could
        // be patched into one with no actions, and a user could be given a
        // password and a hash at once and be left with whichever won
        if let Err(r) = required_fields(&kind, &current) {
            return r;
        }
        if both {
            return bad_request(
                "Please specify either 'hash' or 'password' when creating a new internal user.",
            );
        }
        let mut next = cfg.clone();
        if let Err(r) = put_entry(&mut next, &kind, &name, &current) {
            return r;
        }
        let generation = match install(&store, &mut cfg, next) {
            Ok(g) => g,
            Err(r) => return r,
        };
        store.security.audit.internal_config_written(&caller, &caller.remote_address, &kind);
        generation
    };
    if let Err(r) = published(generation).await {
        return r;
    }
    at_generation(reply(StatusCode::OK, "OK", format!("'{name}' updated.")), generation)
}

pub async fn patch_all(
    State(store): State<Store>,
    Extension(caller): Extension<Caller>,
    Path(kind): Path<String>,
    body: String,
) -> Response {
    if let Err(r) = admin_for(&store, &caller, &kind, "PATCH") {
        return r;
    }
    let mut ops: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return bad_request("Could not parse content of request."),
    };
    if kind == "internalusers"
        && let Err(r) = hash_patched_passwords(&mut ops, None)
    {
        return r;
    }
    if let Err(r) = writable_here() {
        return r;
    }
    let generation = {
        let mut cfg = store.security.config.write();
        if kind == "securityconfig" {
            // This document is the authentication chain. A caller who may write
            // it can add a domain that trusts a header of their choosing -- a
            // proxy authenticator with `internalProxies: .*` turns an
            // unauthenticated request carrying `x-proxy-roles: admin` into full
            // access -- so the reference refuses it unless an operator has
            // explicitly said otherwise, and so does this.
            if !store.security.allow_config_rewrite {
                return bad_request(
                    "Modifying the security configuration through the REST API is not allowed. \
                 Set plugins.security.unsupported.restapi.allow_securityconfig_modification \
                 to true to allow it.",
                );
            }
            let mut current = cfg.document("config");
            if let Err(e) = apply_patch(&mut current, &ops) {
                return bad_request(e);
            }
            let mut next = cfg.clone();
            next.dynamic = current.get("config").cloned().unwrap_or(Value::Object(Map::new()));
            match install(&store, &mut cfg, next) {
                Ok(g) => g,
                Err(r) => return r,
            }
        } else {
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
            // Every entry is written into a copy first. The entries used to go into
            // the live configuration one at a time, and one of them being refused
            // left the ones before it applied here -- in memory, since the refusal
            // returned before anything was saved, so the node answered by a
            // configuration no file held and a restart undid.
            let mut next = cfg.clone();
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
                    if let Err(r) = required_fields(&kind, &v) {
                        return r;
                    }
                    if let Err(r) = put_entry(&mut next, &kind, n, &v) {
                        return r;
                    }
                }
            }
            for n in before_o.keys() {
                if !after.contains_key(n) {
                    remove_entry(&mut next, &kind, n);
                }
            }
            let generation = match install(&store, &mut cfg, next) {
                Ok(g) => g,
                Err(r) => return r,
            };
            store.security.audit.internal_config_written(&caller, &caller.remote_address, &kind);
            generation
        }
    };
    if let Err(r) = published(generation).await {
        return r;
    }
    at_generation(reply(StatusCode::OK, "OK", "Resource updated."), generation)
}

// ---- service accounts and on-behalf-of tokens ----------------------------------------

/// Whether an internal user is a service account.
fn is_service_account(u: &InternalUser) -> bool {
    u.attributes.get("service").map(|v| v.eq_ignore_ascii_case("true")).unwrap_or(false)
}

/// A service account's secret: eight to fifteen letters and digits, with a
/// lowercase letter, an uppercase letter and a digit among them, as the
/// plugin generates it.
fn service_password() -> String {
    const LOWER: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
    const UPPER: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    const DIGIT: &[u8] = b"0123456789";
    let mut bytes: Vec<u8> = Vec::new();
    while bytes.len() < 32 {
        use base64::Engine;
        let token = crate::store::random_token();
        bytes.extend(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(token).unwrap_or_default(),
        );
    }
    let len = 8 + (bytes[0] % 8) as usize;
    let all: Vec<u8> = [LOWER, UPPER, DIGIT].concat();
    let mut out: Vec<u8> = bytes[1..=len].iter().map(|b| all[*b as usize % all.len()]).collect();
    out[0] = LOWER[bytes[len + 1] as usize % LOWER.len()];
    out[1] = UPPER[bytes[len + 2] as usize % UPPER.len()];
    out[2] = DIGIT[bytes[len + 3] as usize % DIGIT.len()];
    // where the three required kinds fall is random too
    for i in (1..out.len()).rev() {
        let j = bytes[len + 4 + (i % 12)] as usize % (i + 1);
        out.swap(i, j);
    }
    String::from_utf8(out).unwrap_or_default()
}

/// `POST _plugins/_security/api/internalusers/{name}/authtoken`: a new secret
/// for an enabled service account, answered in the plugin's words.
///
/// The reference (3.1) answers with the new secret but writes the new hash
/// somewhere it is never read from, so the secret it hands out does not log
/// in. Here the hash is stored, so the credentials answered are credentials.
pub async fn service_authtoken(
    State(store): State<Store>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
) -> Response {
    if let Err(r) = admin_for(&store, &caller, "internalusers", "POST") {
        return r;
    }
    let refused = || bad_request("An auth token could not be generated for the specified account.");
    let usable = {
        let cfg = store.security.config.read();
        match cfg.users.get(&name) {
            None => return not_found("user", &name),
            Some(u) if u.hidden => return not_found("user", &name),
            Some(u) => {
                is_service_account(u)
                    && u.attributes
                        .get("enabled")
                        .map(|v| v.eq_ignore_ascii_case("true"))
                        .unwrap_or(false)
            }
        }
    };
    if !usable {
        return refused();
    }
    if let Err(r) = writable_here() {
        return r;
    }
    let password = service_password();
    let hash = hash_password(&password);
    let generation = {
        let mut cfg = store.security.config.write();
        let before = cfg.document("internalusers");
        let mut next = cfg.clone();
        let Some(u) = next.users.get_mut(&name) else { return refused() };
        u.hash = hash;
        next.merge_documents(&[]);
        let generation = match install(&store, &mut cfg, next) {
            Ok(g) => g,
            Err(r) => return r,
        };
        let after = cfg.document("internalusers");
        store.security.audit.internal_config_written_with(
            &caller,
            &caller.remote_address,
            "internalusers",
            Some(&before),
            Some(&after),
        );
        generation
    };
    if let Err(r) = published(generation).await {
        return r;
    }
    at_generation(
        reply(
            StatusCode::OK,
            "OK",
            format!(
                "'{name}' authtoken generated Basic auth token with user={name}, password={password}"
            ),
        ),
        generation,
    )
}

/// Any method but `POST` on a route that has only that one.
pub async fn post_only(method: axum::http::Method, uri: axum::http::Uri) -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        axum::Json(json!({
            "error": format!("Incorrect HTTP method for uri [{}] and method [{}], allowed: [POST]", uri.path(), method),
            "status": 405,
        })),
    )
        .into_response()
}

fn text_reply(status: StatusCode, message: &str) -> Response {
    let mut r = (status, message.to_string()).into_response();
    r.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/plain; charset=UTF-8"),
    );
    r
}

/// `POST _plugins/_security/api/generateonbehalfoftoken`: a token the caller
/// hands a service to act as the caller for a few minutes. The security layer
/// has already judged `security:obo/create`.
pub async fn generate_obo_token(
    State(store): State<Store>,
    Extension(caller): Extension<Caller>,
    body: String,
) -> Response {
    if !store.security.enabled {
        return disabled();
    }
    let (settings, roles) = {
        let cfg = store.security.config.read();
        let settings = super::obo::OboSettings::from_dynamic(&cfg.dynamic).filter(|s| s.enabled);
        // the roles the caller is mapped to, but not by where it is calling
        // from: a token must not carry a role its holder's address earned
        let own = cfg.users.get(&caller.name).map(|u| u.security_roles.clone()).unwrap_or_default();
        let roles = cfg.map_roles(&caller.name, &caller.backend_roles, &own, "");
        (settings, roles)
    };
    let Some(settings) = settings else {
        return text_reply(
            StatusCode::BAD_REQUEST,
            "The OnBehalfOf token generating API has been disabled, see {link to doc} for more information on this feature.",
        );
    };
    let error = |status: StatusCode, message: &str| {
        (status, axum::Json(json!({"error": message}))).into_response()
    };
    let unexpected = || {
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "An unexpected error occurred. Please check the input and try again.",
        )
    };
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(&body) else {
        return unexpected();
    };
    for key in fields.keys() {
        if !["durationSeconds", "description", "service"]
            .iter()
            .any(|k| k.eq_ignore_ascii_case(key))
        {
            return error(StatusCode::BAD_REQUEST, &format!("Unrecognized parameter: {key}"));
        }
    }
    let seconds = match fields.get("durationSeconds") {
        None => super::obo::DEFAULT_SECONDS,
        Some(Value::Number(n))
            if n.is_i64() && n.as_i64().map(|v| v.abs() <= i32::MAX as i64).unwrap_or(false) =>
        {
            n.as_i64().unwrap_or_default()
        }
        Some(Value::String(s)) if s.parse::<i64>().is_ok() => s.parse::<i64>().unwrap_or_default(),
        Some(_) => return error(StatusCode::BAD_REQUEST, "durationSeconds must be a number."),
    };
    if matches!(fields.get("description"), Some(v) if !v.is_string() && !v.is_null()) {
        return unexpected();
    }
    let service = match fields.get("service") {
        None | Some(Value::Null) => "self-issued".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => return unexpected(),
    };
    if service.is_empty() {
        return unexpected();
    }
    match settings.issue(&caller.name, &service, seconds, &roles) {
        Ok((token, lives)) => ok_json(json!({
            "user": caller.name,
            "authenticationToken": token,
            "durationSeconds": lives,
        })),
        Err(why) if why.starts_with("The expiration") => error(StatusCode::BAD_REQUEST, &why),
        Err(_) => unexpected(),
    }
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
        "custom_attribute_names": caller.attributes.keys().map(|k| attribute_name(k)).collect::<Vec<_>>(),
        "tenants": tenants,
        "roles": caller.roles,
    }))
}

/// The name a caller's attribute goes by: an internal user's own attributes
/// are `attr.internal.*`; what a token or a directory supplied already carries
/// its source (`attr.jwt.*`, `attr.ldap.*`, `ldap.dn`).
fn attribute_name(k: &str) -> String {
    if k.starts_with("attr.") || k.starts_with("ldap.") {
        k.to_string()
    } else {
        format!("attr.internal.{k}")
    }
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
    if let Err(r) = writable_here() {
        return r;
    }
    // both bcrypt steps are taken on a copy, with no lock held: see `put_one`
    let held = store.security.config.read().clone();
    let Some(checked) = held.authenticate(&caller.name, current).map(|u| u.hash.clone()) else {
        return bad_request("Could not validate your current password.");
    };
    if let Err(r) = validate_password(&caller.name, password) {
        return r;
    }
    let hash = hash_password(password);
    let generation = {
        let mut cfg = store.security.config.write();
        // the password checked must still be the caller's
        if cfg.users.get(&caller.name).map(|u| &u.hash) != Some(&checked) {
            return bad_request("Could not validate your current password.");
        }
        let mut next = cfg.clone();
        if let Some(u) = next.users.get_mut(&caller.name) {
            u.hash = hash;
        }
        let generation = match install(&store, &mut cfg, next) {
            Ok(g) => g,
            Err(r) => return r,
        };
        store.security.audit.internal_config_written(
            &caller,
            &caller.remote_address,
            "internalusers",
        );
        generation
    };
    if let Err(r) = published(generation).await {
        return r;
    }
    at_generation(reply(StatusCode::OK, "OK", format!("'{}' updated.", caller.name)), generation)
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
        attrs.insert(attribute_name(k), json!(v));
    }
    ok_json(json!({
        "user": caller.describe(),
        "user_name": caller.name,
        "user_requested_tenant": caller.requested_tenant,
        "remote_address": if caller.remote_address.is_empty() || caller.admin_cert { Value::Null } else { json!(format!("{}:0", caller.remote_address)) },
        "backend_roles": caller.backend_roles,
        "custom_attribute_names": attrs.keys().cloned().collect::<Vec<_>>(),
        "roles": caller.roles,
        "tenants": tenants_of(&cfg, &caller),
        "principal": Value::Null,
        "peer_certificates": "0",
        "sso_logout_url": super::sso_logout_url(&store.security, &caller),
    }))
}

/// `UP` when the node can serve; `DOWN`, with 503, when it cannot -- a node
/// with no cluster manager refuses every write, and a probe that called it
/// healthy sent traffic to a node that would turn it all away.
///
/// Not excused for a node that knows only itself: that is as much a node
/// whose peers never came as a node that is the whole cluster, and the one
/// answered UP while it refused every write. A node that is the whole
/// cluster elects itself in its first moments, which a probe's start period
/// is for.
pub async fn health(State(store): State<Store>) -> Response {
    // a node with security on and no configuration it may let anybody in by
    // is up and serving nobody, as the plugin's own health says of a node
    // whose security index is not initialized
    if store.security.enabled && store.security.standing() == super::Standing::NotInitialized {
        let mut r =
            ok_json(json!({"message": "Not initialized", "mode": "strict", "status": "DOWN"}));
        *r.status_mut() = axum::http::StatusCode::SERVICE_UNAVAILABLE;
        return r;
    }
    if !crate::cluster::has_manager() {
        let mut r =
            ok_json(json!({"message": "no cluster-manager", "mode": "strict", "status": "DOWN"}));
        *r.status_mut() = axum::http::StatusCode::SERVICE_UNAVAILABLE;
        return r;
    }
    ok_json(json!({"message": Value::Null, "mode": "strict", "status": "UP",
        "settings": {"plugins.security.cache.ttl_minutes": 60}}))
}

pub async fn permissions_info(
    State(store): State<Store>,
    Extension(caller): Extension<Caller>,
) -> Response {
    if !store.security.enabled {
        return disabled();
    }
    // an admin certificate is not a role with REST API access, and the
    // plugin says so here even though it lets the certificate through
    let allowed = !caller.admin_cert && store.security.may_administer(&caller);
    ok_json(json!({
        "user": caller.describe(),
        "user_name": caller.name,
        "has_api_access": allowed,
        "disabled_endpoints": {},
    }))
}

/// The node's own certificates, as `ssl/certs` describes them.
pub async fn certs(State(store): State<Store>, Extension(caller): Extension<Caller>) -> Response {
    if let Err(r) = admin_for(&store, &caller, "SSL", "GET") {
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
    let cert_path =
        tls.cert.clone().unwrap_or_else(|| crate::tls::config_dir().join("certs").join("node.pem"));
    let list = match std::fs::read(&cert_path).ok() {
        Some(pem) => describe_certs(&pem),
        None => Vec::new(),
    };
    ok_json(json!({"http_certificates_list": list, "transport_certificates_list": list}))
}

fn describe_certs(pem: &[u8]) -> Vec<Value> {
    let mut out = Vec::new();
    for item in x509_parser::pem::Pem::iter_from_buffer(pem).flatten() {
        let Ok(cert) = item.parse_x509() else { continue };
        // Java lists each name as [type, value]: 2 a DNS name, 7 an address,
        // 1 an e-mail address, 6 a URI
        let san: Vec<String> = cert
            .subject_alternative_name()
            .ok()
            .flatten()
            .map(|s| {
                s.value
                    .general_names
                    .iter()
                    .map(|g| {
                        use x509_parser::extensions::GeneralName::*;
                        match g {
                            DNSName(d) => format!("[2, {d}]"),
                            IPAddress(b) => format!(
                                "[7, {}]",
                                if b.len() == 4 {
                                    b.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(".")
                                } else {
                                    b.chunks(2)
                                        .map(|c| {
                                            format!(
                                                "{:x}",
                                                u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)])
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                        .join(":")
                                }
                            ),
                            RFC822Name(m) => format!("[1, {m}]"),
                            URI(u) => format!("[6, {u}]"),
                            RegisteredID(o) => format!("[8, {o}]"),
                            DirectoryName(n) => format!("[4, {n}]"),
                            _ => "[0, ]".to_string(),
                        }
                    })
                    .collect()
            })
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

/// A Unix timestamp as `2026-01-01T00:00:00Z`.
pub fn rfc3339_no_millis(ts: i64) -> String {
    rfc3339(ts).replace(".000Z", "Z")
}

/// The SAML token exchange: `{"SAMLResponse": ..., "RequestId": ...}` in,
/// `{"authorization": "bearer <jwt>"}` out. A response that does not hold
/// up is refused with an empty 401, as the plugin refuses it; a request
/// with no response at all fails as the plugin's authentication fails.
pub async fn authtoken(State(store): State<Store>, body: String) -> Response {
    if !store.security.enabled {
        return disabled();
    }
    let Some(saml) = store.security.chain.read().clone().saml() else {
        return (StatusCode::UNAUTHORIZED, "Authentication finally failed").into_response();
    };
    let parsed: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, "JSON could not be parsed").into_response(),
    };
    match saml.exchange(&parsed) {
        Ok(token) => ok_json(json!({"authorization": format!("bearer {token}")})),
        Err((400, _)) => {
            (StatusCode::UNAUTHORIZED, "Authentication finally failed").into_response()
        }
        Err((_, why)) => {
            if std::env::var("VELOSEARCH_AUTH_DEBUG").is_ok() {
                eprintln!("saml: {why}");
            }
            (StatusCode::UNAUTHORIZED, "").into_response()
        }
    }
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
pub async fn unknown(
    State(store): State<Store>,
    Extension(caller): Extension<Caller>,
    method: axum::http::Method,
    uri: axum::http::Uri,
) -> Response {
    if !store.security.enabled {
        return disabled();
    }
    if !store.security.may_administer(&caller) {
        return api_forbidden(&caller);
    }
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({"error": format!("no handler found for uri [{}] and method [{}]", uri.path(), method)})),
    )
        .into_response()
}

pub fn _unused() -> Response {
    unauthorized()
}

/// `GET _plugins/_security/api/audit`
pub async fn audit_get(
    State(store): State<Store>,
    Extension(caller): Extension<Caller>,
) -> Response {
    if let Err(r) = admin_for(&store, &caller, "AUDIT", "GET") {
        return r;
    }
    store.security.audit.internal_config_read(&caller, &caller.remote_address, "audit");
    ok_json(store.security.audit.api_view())
}

/// `PUT _plugins/_security/api/audit/config`
pub async fn audit_put(
    State(store): State<Store>,
    Extension(caller): Extension<Caller>,
    body: String,
) -> Response {
    if let Err(r) = admin_for(&store, &caller, "AUDIT", "PUT") {
        return r;
    }
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"status": "error", "reason": "Could not parse content of request."})),
        )
            .into_response();
    };
    if let Err(why) = super::audit::AuditConfig::validate(&v) {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"status": "error", "reason": why})))
            .into_response();
    }
    let next = super::audit::AuditConfig::from_json(&v);
    if let Some(conflict) = readonly_conflict(&store, &next) {
        return conflict;
    }
    store.security.audit.store(next);
    store.security.audit.internal_config_written(&caller, &caller.remote_address, "audit");
    reply(StatusCode::OK, "OK", "'config' updated.")
}

/// `PATCH _plugins/_security/api/audit`
pub async fn audit_patch(
    State(store): State<Store>,
    Extension(caller): Extension<Caller>,
    body: String,
) -> Response {
    if let Err(r) = admin_for(&store, &caller, "AUDIT", "PATCH") {
        return r;
    }
    let Ok(ops) = serde_json::from_str::<Value>(&body) else {
        return bad_request("Could not parse content of request.");
    };
    let mut current = store.security.audit.api_view();
    let before = current.clone();
    if let Err(e) = apply_patch(&mut current, &ops) {
        return bad_request(e);
    }
    if current == before {
        return reply(StatusCode::OK, "OK", "No updates required");
    }
    let cfg = current.get("config").cloned().unwrap_or(Value::Null);
    if let Err(why) = super::audit::AuditConfig::validate(&cfg) {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"status": "error", "reason": why})))
            .into_response();
    }
    let next = super::audit::AuditConfig::from_json(&cfg);
    if let Some(conflict) = readonly_conflict(&store, &next) {
        return conflict;
    }
    store.security.audit.store(next);
    store.security.audit.internal_config_written(&caller, &caller.remote_address, "audit");
    reply(StatusCode::OK, "OK", "Resource updated.")
}

/// A read-only path (`plugins.security.audit.config.readonly`) changed.
fn readonly_conflict(store: &Store, next: &super::audit::AuditConfig) -> Option<Response> {
    let current = store.security.audit.current().to_json();
    let proposed = next.to_json();
    for path in &store.security.audit.readonly {
        let p = if path.starts_with('/') {
            path.clone()
        } else {
            format!("/{}", path.replace('.', "/"))
        };
        if current.pointer(&p) != proposed.pointer(&p) {
            return Some(reply(
                StatusCode::CONFLICT,
                "CONFLICT",
                "Attempted to update read-only property.",
            ));
        }
    }
    None
}

/// The plugin's answer to a method its audit routes do not take.
pub async fn audit_wrong_method(method: axum::http::Method, uri: axum::http::Uri) -> Response {
    let allowed = if uri.path().ends_with("/audit/config") { "[PUT]" } else { "[PATCH, GET]" };
    (
        StatusCode::METHOD_NOT_ALLOWED,
        axum::Json(json!({
            "error": format!("Incorrect HTTP method for uri [{}] and method [{}], allowed: {allowed}", uri.path(), method),
            "status": 405,
        })),
    )
        .into_response()
}
