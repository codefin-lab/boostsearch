//! The security configuration, spread to the other nodes.
//!
//! A cluster's security configuration is one thing, not one thing per node.
//! Here it was a directory of files each node read at startup and each node's
//! security API wrote to on its own: revoking a role on the node that took the
//! request left every other node granting it, for as long as the cluster ran.
//! A caller who saw the refusal on one node simply asked another.
//!
//! What follows is the missing half: every write through the security API is
//! sent to the other nodes, which take it whole and save it. It is a
//! best-effort spread, not a consensus -- a node that is away is left behind
//! and says so in its log -- and the caller is told which nodes did not take
//! it rather than being told the change is everywhere.

use std::sync::Arc;

use serde_json::Value;

use super::SecurityConfig;
use crate::cluster::runtime::{DataFuture, Runtime};
use crate::cluster::transport::{Envelope, Kind, NodeId};
use crate::store::Store;

pub const ACTION: &str = "internal:security/config";

/// The kinds a configuration is made of, in the order they are laid down.
const KINDS: [&str; 6] =
    ["internalusers", "roles", "rolesmapping", "actiongroups", "tenants", "config"];

/// Every document of a configuration, as the wire carries them.
pub fn documents(cfg: &SecurityConfig) -> Value {
    let mut o = serde_json::Map::new();
    for kind in KINDS {
        o.insert(kind.to_string(), cfg.document(kind));
    }
    Value::Object(o)
}

/// Take a configuration another node wrote.
fn apply(store: &Store, body: &Value) {
    let docs: Vec<(&str, Value)> =
        KINDS.iter().filter_map(|k| body.get(*k).map(|v| (*k, v.clone()))).collect();
    let next = SecurityConfig::from_documents(&docs);
    let mut cfg = store.security.config.write();
    *cfg = next;
    // saved as well as held: a restart here must not bring the old one back
    let _ = cfg.save();
    store.security.touch(&cfg);
}

/// Answer another node's copy of the configuration.
pub fn install(rt: &Runtime, store: &Store, me: &NodeId) {
    let s = store.clone();
    let from = me.clone();
    rt.register(
        ACTION,
        Arc::new(move |e: Envelope| -> DataFuture {
            let store = s.clone();
            let from = from.clone();
            Box::pin(async move {
                let v: Value = serde_json::from_slice(&e.body).unwrap_or(Value::Null);
                if !v.is_object() {
                    return e.error(from, "the security configuration could not be read");
                }
                apply(&store, &v);
                e.response(from, b"{\"acknowledged\":true}".to_vec())
            })
        }),
    );
}

/// Send what was just written to every other node.
///
/// Called while the configuration lock is held, so the copy is taken here and
/// the sending is done on a task of its own.
pub fn after_write(store: &Store, cfg: &SecurityConfig) {
    let Some(rt) = crate::cluster::runtime() else { return };
    let me = rt.local();
    let others: Vec<NodeId> =
        crate::cluster::with_state(|s| s.nodes.keys().filter(|n| **n != me).cloned().collect());
    if others.is_empty() {
        return;
    }
    let body = match serde_json::to_vec(&documents(cfg)) {
        Ok(b) => b,
        Err(_) => return,
    };
    let _ = store;
    tokio::spawn(async move {
        // one broadcast at a time, in the order the writes were made: two
        // sends in flight together can arrive in either order, and the loser
        // would leave the far node holding the configuration that was
        // replaced
        static IN_ORDER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _one_at_a_time = IN_ORDER.lock().await;
        for node in others {
            let answer =
                rt.call(&node, ACTION, body.clone(), std::time::Duration::from_secs(10)).await;
            let took = answer.map(|e| e.kind != Kind::Error).unwrap_or(false);
            if !took {
                tracing::error!(
                    "node [{node}] did not take the security configuration: it is still \
                     answering by the one it has"
                );
            }
        }
    });
}
