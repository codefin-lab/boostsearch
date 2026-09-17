//! Weighted routing and decommissioning, by awareness attribute.
//!
//! A real cluster spread over zones can send a share of its searches to each
//! zone (`_cluster/routing/awareness/<attr>/weights`) and take a whole zone
//! out of service (`_cluster/decommission/awareness/<attr>/<value>`). There
//! are no zones here to spread copies over, so neither changes where a search
//! goes. What both do keep is their metadata: the weights and the
//! decommission status are the cluster's, published with the rest of what the
//! manager holds and written down, so a manager change or a restart still
//! answers what was put -- which is the half of these APIs a client depends
//! on, and the half a 501 gave nothing of.
//!
//! The validation is the reference's, in the reference's order, so a request
//! that a real cluster would refuse is refused here with the same words.

use super::*;

/// The awareness attributes the cluster is configured for, as
/// `cluster.routing.allocation.awareness.attributes` names them.
fn awareness_attributes(store: &Store) -> Vec<String> {
    flat_setting(store, "cluster.routing.allocation.awareness.attributes")
        .map(|v| split_list(&v))
        .unwrap_or_default()
}

/// The values forced for one attribute, as
/// `cluster.routing.allocation.awareness.force.<attr>.values` names them.
fn forced_values(store: &Store, attribute: &str) -> Option<Vec<String>> {
    let key = format!("cluster.routing.allocation.awareness.force.{attribute}.values");
    flat_setting(store, &key).map(|v| split_list(&v)).filter(|v: &Vec<String>| !v.is_empty())
}

/// Every attribute the forced awareness settings name, for the message that
/// prints them.
fn forced_attributes(store: &Store) -> Vec<String> {
    let mut flat = serde_json::Map::new();
    let settings = store.cluster_settings();
    for scope in ["persistent", "transient"] {
        if let Some(o) = settings.get(scope) {
            super::settings::flatten_cluster_settings(o, "", &mut flat);
        }
    }
    let mut names: Vec<String> = flat
        .keys()
        .filter_map(|k| k.strip_prefix("cluster.routing.allocation.awareness.force."))
        .filter_map(|rest| rest.strip_suffix(".values"))
        .map(|s| s.to_string())
        .collect();
    names.sort();
    names.dedup();
    names.retain(|n| forced_values(store, n).is_some());
    names
}

/// One cluster setting by its dotted name, however the caller wrote it.
fn flat_setting(store: &Store, key: &str) -> Option<String> {
    let settings = store.cluster_settings();
    for scope in ["transient", "persistent"] {
        let Some(o) = settings.get(scope) else { continue };
        let mut flat = serde_json::Map::new();
        super::settings::flatten_cluster_settings(o, "", &mut flat);
        if let Some(v) = flat.get(key).and_then(|v| v.as_str()) {
            return Some(v.to_string());
        }
    }
    None
}

fn split_list(v: &str) -> Vec<String> {
    v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// `Validation Failed: 1: ...;`, as an action request's refusal reads.
fn validation_failed(reasons: &[String]) -> Response {
    let mut out = String::from("Validation Failed:");
    for (i, r) in reasons.iter().enumerate() {
        out.push_str(&format!(" {}: {r};", i + 1));
    }
    err(StatusCode::BAD_REQUEST, "action_request_validation_exception", out)
}

/// The version the metadata is at, as the version check compares against:
/// -1 while there is no metadata at all.
const INITIAL_VERSION: i64 = -1;
/// The version a request that named none carries, which matches nothing.
const VERSION_UNSET: i64 = -2;

fn current_version(store: &Store) -> i64 {
    store
        .weighted_routing()
        .and_then(|m| m.get("_version").and_then(|v| v.as_i64()))
        .unwrap_or(INITIAL_VERSION)
}

/// The weights set, by the attribute they were set for.
fn current_weights(store: &Store) -> Option<(String, serde_json::Map<String, Value>)> {
    let meta = store.weighted_routing()?;
    let by_attribute = meta.get("awareness")?.as_object()?.clone();
    let (attribute, weights) = by_attribute.into_iter().next()?;
    Some((attribute, weights.as_object().cloned().unwrap_or_default()))
}

fn version_conflict(requested: i64, current: i64) -> Response {
    err(
        StatusCode::CONFLICT,
        "unsupported_weighted_routing_state_exception",
        format!(
            "requested version is {requested} but cluster weighted routing metadata is at a \
             different version {current} "
        ),
    )
}

/// The weights as the metadata and the answer write them: a double, printed
/// the way a double prints, so `1` comes back as `1.0`.
fn weight_text(w: f64) -> String {
    if w == w.trunc() && w.abs() < 1e15 { format!("{w:.1}") } else { format!("{w}") }
}

/// `GET /_cluster/routing/awareness/{attribute}/weights`
pub async fn get_weighted_routing(
    State(store): State<Store>,
    Path(attribute): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    if let Some(refusal) = bad_attribute(&store, &attribute) {
        return refusal;
    }
    let Some(meta) = store.weighted_routing() else { return respond(&p, json!({})) };
    let weights: serde_json::Map<String, Value> = meta
        .pointer(&format!("/awareness/{attribute}"))
        .and_then(|v| v.as_object())
        .map(|o| {
            o.iter()
                .map(|(k, v)| (k.clone(), json!(weight_text(v.as_f64().unwrap_or(0.0)))))
                .collect()
        })
        .unwrap_or_default();
    respond(
        &p,
        json!({
            "weights": Value::Object(weights),
            "_version": meta.get("_version").cloned().unwrap_or(json!(INITIAL_VERSION)),
            // the answer says the node found a manager to ask, which it did
            "discovered_cluster_manager": true,
        }),
    )
}

/// An attribute the cluster is not aware of, as both the read and the write
/// of the weights refuse it.
fn bad_attribute(store: &Store, attribute: &str) -> Option<Response> {
    let known = awareness_attributes(store);
    (!known.iter().any(|a| a == attribute)).then(|| {
        validation_failed(&[format!(
            "invalid awareness attribute {attribute} requested for weighted routing"
        )])
    })
}

/// `PUT /_cluster/routing/awareness/{attribute}/weights`
pub async fn put_weighted_routing(
    State(store): State<Store>,
    Path(attribute): Path<String>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let parse = |reason: String| err(StatusCode::BAD_REQUEST, "parse_exception", reason);
    // a body naming nothing is a body that asked for nothing. The reference
    // answers an empty object with this and a wholly absent body with a Java
    // NullPointerException; the absent body is the empty one here.
    let asked: Value = match serde_json::from_str(body.trim()) {
        Ok(Value::Object(o)) if !o.is_empty() => Value::Object(o),
        Ok(_) | Err(_) if body.trim().is_empty() => return parse("Empty request body".into()),
        Ok(Value::Object(_)) => return parse("Empty request body".into()),
        Ok(_) => return parse("Empty request body".into()),
        Err(e) => return parse(format!("failed to parse weighted routing request object: {e}")),
    };
    let mut version = VERSION_UNSET;
    let mut weights: serde_json::Map<String, Value> = serde_json::Map::new();
    for (key, value) in asked.as_object().into_iter().flatten() {
        match key.as_str() {
            "_version" => {
                version = match value.as_i64() {
                    Some(v) => v,
                    None => return parse("failed to parse weighted routing request object".into()),
                }
            }
            "weights" => {
                let Some(o) = value.as_object() else {
                    return parse(
                        "failed to parse weighted routing request object  [weights], expected \
                         object"
                            .into(),
                    );
                };
                for (attr_value, weight) in o {
                    // only text is a weight: a bare number is refused, as the
                    // reference refuses it
                    let Some(text) = weight.as_str() else {
                        return parse(
                            "failed to parse weighted routing request attribute [null], unknown \
                             type"
                                .into(),
                        );
                    };
                    let Ok(w) = text.parse::<f64>() else {
                        return err(
                            StatusCode::BAD_REQUEST,
                            "number_format_exception",
                            format!("For input string: \"{text}\""),
                        );
                    };
                    weights.insert(attr_value.clone(), json!(w));
                }
            }
            other => {
                return parse(format!("failed to parse weighted routing request object [{other}]"));
            }
        }
    }
    // what the request itself is wrong about, before the cluster is asked
    let mut wrong: Vec<String> = Vec::new();
    if attribute.is_empty() {
        wrong.push("Attribute name is missing".into());
    }
    if weights.is_empty() {
        wrong.push("Weights are missing".into());
    }
    if version == VERSION_UNSET {
        wrong.push("Version is missing".into());
    }
    if !wrong.is_empty() {
        return validation_failed(&wrong);
    }
    let zeroes = weights.values().filter(|v| v.as_f64() == Some(0.0)).count();
    if zeroes > weights.len() / 2 {
        return validation_failed(&[format!(
            "There are too many attribute values [{}] given zero weight [{}]. Maximum expected \
             number of routing weights having zero weight is [{}]",
            printed_weights(&weights),
            zeroes,
            weights.len() / 2
        )]);
    }
    if let Some(refusal) = bad_attribute(&store, &attribute) {
        return refusal;
    }
    // every value a forced awareness setting names must be given a weight,
    // and after they are all counted no more than half may be zero
    let mut all: Vec<String> = forced_values(&store, &attribute).unwrap_or_default();
    for node in crate::cluster::current_state().nodes.values() {
        if let Some(v) = node.attributes.get(&attribute)
            && !all.iter().any(|a| a == v)
        {
            all.push(v.clone());
        }
    }
    let mut zero_of_all = 0;
    for value in &all {
        let Some(w) = weights.get(value).and_then(|v| v.as_f64()) else {
            return err(
                StatusCode::CONFLICT,
                "unsupported_weighted_routing_state_exception",
                format!(
                    "weight for [{value}] is not set and it is part of forced awareness value or \
                     a routing node has this attribute."
                ),
            );
        };
        if w == 0.0 {
            zero_of_all += 1;
        }
    }
    if zero_of_all > all.len() / 2 {
        return validation_failed(&[format!(
            "There are too many discovered attribute values [{}] given zero weight [{}]. Maximum \
             expected number of routing weights having zero weight is [{}]",
            printed_weights(&weights),
            zero_of_all,
            all.len() / 2
        )]);
    }
    if let Some(refusal) = weights_fit_decommission(&store, &attribute, &weights) {
        return refusal;
    }
    let current = current_version(&store);
    if version != current {
        return version_conflict(version, current);
    }
    // unchanged weights are not a new version: the reference leaves the state
    // alone and still acknowledges
    let same =
        current_weights(&store).map(|(a, w)| a == attribute && w == weights).unwrap_or(false);
    if !same {
        store.set_awareness(
            "weighted_routing",
            Some(json!({
                "awareness": {attribute.clone(): Value::Object(weights)},
                "_version": current + 1,
            })),
        );
    }
    respond(&p, json!({"acknowledged": true}))
}

/// The weights as the reference prints a map of them in a message.
fn printed_weights(weights: &serde_json::Map<String, Value>) -> String {
    let inner: Vec<String> = weights
        .iter()
        .map(|(k, v)| format!("{k}={}", weight_text(v.as_f64().unwrap_or(0.0))))
        .collect();
    format!("{{{}}}", inner.join(", "))
}

/// A zone being decommissioned must be weighed to zero, and the weights of
/// another attribute may not be touched while one is going on.
fn weights_fit_decommission(
    store: &Store,
    attribute: &str,
    weights: &serde_json::Map<String, Value>,
) -> Option<Response> {
    let meta = store.decommission()?;
    if meta.get("status").and_then(|s| s.as_str()) == Some("failed") {
        return None;
    }
    let (name, value) = decommission_attribute(&meta)?;
    let conflict = |reason: String| {
        Some(err(StatusCode::CONFLICT, "unsupported_weighted_routing_state_exception", reason))
    };
    if name != attribute {
        return conflict(format!(
            "decommission action ongoing for attribute [{name}], cannot update weight for \
             [{attribute}]"
        ));
    }
    match weights.get(&value).and_then(|v| v.as_f64()) {
        None => conflict(format!(
            "weight for [{value}] is not specified. Please specify its weight to [0.0] as it is \
             under decommission action"
        )),
        Some(w) if w != 0.0 => conflict(format!(
            "weight for [{value}] must be set to [0.0] as it is under decommission action"
        )),
        Some(_) => None,
    }
}

/// `DELETE /_cluster/routing/awareness/weights` and the same under an
/// attribute: the weights go, the version moves on.
pub async fn delete_weighted_routing(
    State(store): State<Store>,
    attribute: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let named = attribute.map(|Path(a)| a);
    let mut version = VERSION_UNSET;
    if !body.trim().is_empty() {
        let asked: Value = match serde_json::from_str(body.trim()) {
            Ok(v) => v,
            Err(_) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "parse_exception",
                    "failed to parse delete weighted routing request body",
                );
            }
        };
        let Some(o) = asked.as_object().filter(|o| !o.is_empty()) else {
            return err(StatusCode::BAD_REQUEST, "parse_exception", "Empty request body");
        };
        for (key, value) in o {
            if key != "_version" {
                return err(
                    StatusCode::BAD_REQUEST,
                    "parse_exception",
                    format!(
                        "failed to parse delete weighted routing request body [{key}], unknown type"
                    ),
                );
            }
            // the body is read as text, so a number and its digits are one
            version = match value.as_i64().or_else(|| value.as_str().and_then(|s| s.parse().ok())) {
                Some(v) => v,
                None => {
                    return err(
                        StatusCode::BAD_REQUEST,
                        "parse_exception",
                        "failed to parse delete weighted routing request body",
                    );
                }
            };
        }
    }
    let current = current_version(&store);
    if version != current {
        return version_conflict(version, current);
    }
    let set_for = current_weights(&store).map(|(a, _)| a);
    let ours = match (&set_for, &named) {
        (Some(_), None) => true,
        (Some(a), Some(n)) => a == n,
        (None, _) => false,
    };
    if !ours {
        return err(
            StatusCode::NOT_FOUND,
            "resource_not_found_exception",
            format!(
                "weighted routing metadata does not have weights set for awareness attribute {}",
                named.unwrap_or_else(|| "null".into())
            ),
        );
    }
    store
        .set_awareness("weighted_routing", Some(json!({"awareness": {}, "_version": current + 1})));
    respond(&p, json!({"acknowledged": true}))
}

/// The attribute and value a decommission was asked for.
fn decommission_attribute(meta: &Value) -> Option<(String, String)> {
    let (name, value) = meta.get("awareness")?.as_object()?.iter().next()?;
    Some((name.clone(), value.as_str().unwrap_or_default().to_string()))
}

/// `GET /_cluster/decommission/awareness/{attribute}/_status`
pub async fn get_decommission_awareness(
    State(store): State<Store>,
    Path(attribute): Path<String>,
    Query(p): Query<Params>,
) -> Response {
    let asked = store
        .decommission()
        .as_ref()
        .and_then(decommission_attribute)
        .filter(|(name, _)| *name == attribute);
    match (asked, store.decommission()) {
        (Some((_, value)), Some(meta)) => {
            let status = meta.get("status").and_then(|s| s.as_str()).unwrap_or("init");
            respond(&p, json!({value: status}))
        }
        _ => respond(&p, json!({})),
    }
}

fn decommission_failed(name: &str, value: &str, reason: &str) -> Response {
    err(
        StatusCode::BAD_REQUEST,
        "decommissioning_failed_exception",
        format!(
            "[DecommissionAttribute{{attributeName='{name}', attributeValue='{value}'}}] {reason}"
        ),
    )
}

/// `PUT /_cluster/decommission/awareness/{attribute}/{value}`
pub async fn put_decommission_awareness(
    State(store): State<Store>,
    Path((name, value)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Response {
    let known = awareness_attributes(&store);
    let forced = forced_attributes(&store);
    if known.is_empty() {
        return decommission_failed(&name, &value, "awareness attribute not set to the cluster.");
    }
    if !known.contains(&name) {
        return decommission_failed(
            &name,
            &value,
            "invalid awareness attribute requested for decommissioning",
        );
    }
    if !forced.contains(&name) {
        let printed: Vec<String> = forced
            .iter()
            .map(|a| format!("{a}={:?}", forced_values(&store, a).unwrap_or_default()))
            .collect();
        return decommission_failed(
            &name,
            &value,
            &format!(
                "forced awareness attribute [{{{}}}] doesn't have the decommissioning attribute",
                printed.join(", ")
            ),
        );
    }
    // a decommission already registered says whether this one may proceed
    if let Some(meta) = store.decommission() {
        let status = meta.get("status").and_then(|s| s.as_str()).unwrap_or("init").to_string();
        let same = decommission_attribute(&meta) == Some((name.clone(), value.clone()));
        if status != "failed" {
            let reason = if same {
                match status.as_str() {
                    "init" => "same request is already in status [INIT]".to_string(),
                    other => {
                        format!("same request is already in status [{}]", other.to_uppercase())
                    }
                }
            } else {
                let (n, v) = decommission_attribute(&meta).unwrap_or_default();
                let held =
                    format!("DecommissionAttribute{{attributeName='{n}', attributeValue='{v}'}}");
                match status.as_str() {
                    "successful" => format!(
                        "one awareness attribute [{held}] already successfully decommissioned, \
                         recommission before triggering another decommission"
                    ),
                    _ => format!(
                        "there's an inflight decommission request for attribute [{held}] is in \
                         progress, cannot process this request"
                    ),
                }
            };
            return decommission_failed(&name, &value, &reason);
        }
    }
    // the zone to go must have been weighed out of the way first
    let Some((weighed_for, weights)) = current_weights(&store) else {
        return decommission_failed(
            &name,
            &value,
            "no weights are set to the attribute. Please set appropriate weights before \
             triggering decommission action",
        );
    };
    if weighed_for != name {
        return decommission_failed(
            &name,
            &value,
            &format!("no weights are specified to attribute [{name}]"),
        );
    }
    if let Some(w) = weights.get(&value).and_then(|v| v.as_f64())
        && w != 0.0
    {
        return decommission_failed(
            &name,
            &value,
            &format!(
                "weight for decommissioned attribute is expected to be [0.0] but found [{}]",
                weight_text(w)
            ),
        );
    }
    // which manager-eligible nodes would go with the zone. A decommission
    // works by taking them out of the voting configuration first, and the
    // reference refuses a request that names none -- the exclusion it would
    // then ask for names no node at all.
    let state = crate::cluster::current_state();
    let me = crate::cluster::identity();
    let going: Vec<String> = state
        .nodes
        .values()
        .filter(|n| n.attributes.get(&name).map(String::as_str) == Some(value.as_str()))
        .filter(|n| n.is_cluster_manager_eligible())
        .map(|n| n.id.as_str().to_string())
        .collect();
    if going.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "Please set node identifiers correctly. One and only one of [node_name], \
             [node_names] and [node_ids] has to be set",
        );
    }
    let request_id = crate::cluster::transport::NodeId::random().0;
    let record = |status: &str| {
        json!({
            "awareness": {name.clone(): value.clone()},
            "status": status,
            "requestID": request_id.clone(),
        })
    };
    // the leader cannot lead the cluster out of its own zone: it would have
    // to abdicate to a node that is going too
    let others_left = state
        .nodes
        .values()
        .any(|n| !going.iter().any(|g| *g == n.id.as_str()) && n.is_cluster_manager_eligible());
    if going.iter().any(|g| *g == me.id.as_str()) && !others_left {
        store.set_awareness("decommission", Some(record("failed")));
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "illegal_state_exception",
            "unexpected state encountered [local node is to-be-decommissioned leader] while \
             executing decommission request",
        );
    }
    store.set_awareness("decommission", Some(record("successful")));
    respond(&p, json!({"acknowledged": true}))
}

/// `DELETE /_cluster/decommission/awareness` -- the zone is in service again.
pub async fn delete_decommission_awareness(
    State(store): State<Store>,
    Query(p): Query<Params>,
) -> Response {
    store.set_awareness("decommission", None);
    respond(&p, json!({"acknowledged": true}))
}
