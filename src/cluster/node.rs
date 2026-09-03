//! Who this node is.
//!
//! The node id is drawn once and kept in the data directory, as OpenSearch
//! keeps its `NodeMetadata`, so the node is the same node after a restart;
//! the ephemeral id is fresh every start. Name, roles and addresses come
//! from the settings, with OpenSearch's defaults.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::transport::NodeId;

#[derive(Clone, Debug)]
pub struct NodeIdentity {
    pub id: NodeId,
    pub ephemeral_id: NodeId,
    pub name: String,
    pub roles: Vec<String>,
    /// the address other nodes reach this one at
    pub transport_address: String,
    /// the address the transport listener binds
    pub transport_bind: String,
    pub host: String,
    pub attributes: serde_json::Map<String, Value>,
    /// `cluster.name`
    pub cluster_name: String,
    /// `discovery.seed_hosts`
    pub seed_hosts: Vec<String>,
    /// `cluster.initial_cluster_manager_nodes` (or the older `initial_master_nodes`)
    pub initial_cluster_manager_nodes: Vec<String>,
    /// `discovery.type`: `single-node` forms a cluster of one without waiting
    pub single_node: bool,
    /// the uuid of the cluster this node's data belongs to
    pub cluster_uuid: String,
}

fn setting(settings: &Value, key: &str) -> Option<String> {
    crate::tls::node_setting(settings, key).filter(|s| !s.is_empty())
}

fn list_setting(settings: &Value, key: &str) -> Vec<String> {
    match settings.pointer(&format!("/{}", key.replace('.', "/"))).or_else(|| settings.get(key)) {
        Some(Value::Array(a)) => {
            a.iter().filter_map(|v| v.as_str()).map(|s| s.to_string()).collect()
        }
        Some(Value::String(s)) => {
            s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
        }
        _ => setting(settings, key)
            .map(|s| {
                s.split(',')
                    .map(|x| x.trim().trim_matches(['[', ']', '"', '\'']).to_string())
                    .filter(|x| !x.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
    }
}

impl NodeIdentity {
    /// The identity for a node whose data lives at `data_dir` (none for a
    /// node in memory, whose id is fresh each start).
    pub fn load(settings: &Value, data_dir: Option<&Path>, http_addr: &str) -> NodeIdentity {
        let (id, cluster_uuid) = match data_dir {
            Some(dir) => (persisted_id(dir), persisted(dir, "cluster.uuid")),
            None => (NodeId::random(), NodeId::random().0),
        };
        let host_default = http_addr
            .rsplit_once(':')
            .map(|(h, _)| h.to_string())
            .unwrap_or_else(|| "127.0.0.1".into());
        let host = setting(settings, "network.host").unwrap_or(host_default);
        let port = setting(settings, "transport.port").unwrap_or_else(|| "9300".into());
        let bind_host = setting(settings, "transport.bind_host").unwrap_or_else(|| host.clone());
        let publish_host =
            setting(settings, "transport.publish_host").unwrap_or_else(|| host.clone());
        let name = setting(settings, "node.name").unwrap_or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| "boostsearch".into())
        });
        let roles = {
            let r = list_setting(settings, "node.roles");
            if r.is_empty() {
                vec![
                    "cluster_manager".into(),
                    "data".into(),
                    "ingest".into(),
                    "remote_cluster_client".into(),
                ]
            } else {
                r
            }
        };
        let mut attributes = serde_json::Map::new();
        if let Some(Value::Object(attrs)) = settings.pointer("/node/attr") {
            for (k, v) in attrs {
                attributes.insert(
                    k.clone(),
                    Value::String(
                        v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string()),
                    ),
                );
            }
        }
        if let Ok(env) = std::env::var("BOOSTSEARCH_NODE_ATTRS") {
            for pair in env.split(',') {
                if let Some((k, v)) = pair.split_once('=') {
                    attributes.insert(k.trim().to_string(), Value::String(v.trim().to_string()));
                }
            }
        }
        let seed_hosts = list_setting(settings, "discovery.seed_hosts");
        let mut initial = list_setting(settings, "cluster.initial_cluster_manager_nodes");
        if initial.is_empty() {
            initial = list_setting(settings, "cluster.initial_master_nodes");
        }
        let discovery_type = setting(settings, "discovery.type").unwrap_or_default();
        NodeIdentity {
            id,
            ephemeral_id: NodeId::random(),
            name,
            roles,
            transport_address: format!("{publish_host}:{port}"),
            transport_bind: format!("{bind_host}:{port}"),
            host,
            attributes,
            cluster_name: setting(settings, "cluster.name").unwrap_or_else(|| "boostsearch".into()),
            seed_hosts,
            initial_cluster_manager_nodes: initial,
            single_node: discovery_type == "single-node",
            cluster_uuid,
        }
    }

    pub fn is_cluster_manager_eligible(&self) -> bool {
        self.roles.iter().any(|r| r == "cluster_manager" || r == "master")
    }

    pub fn is_data(&self) -> bool {
        self.roles.iter().any(|r| r == "data" || r.starts_with("data_"))
    }
}

fn id_file(dir: &Path) -> PathBuf {
    dir.join("_state").join("node.id")
}

/// A random id kept under `_state/<name>`, made on first use.
fn persisted(dir: &Path, name: &str) -> String {
    let path = dir.join("_state").join(name);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let t = text.trim();
        if t.len() == 22 {
            return t.to_string();
        }
    }
    let id = NodeId::random().0;
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(dir));
    let _ = std::fs::write(&path, &id);
    id
}

/// The id kept for this data directory, made on first use.
fn persisted_id(dir: &Path) -> NodeId {
    let path = id_file(dir);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let t = text.trim();
        if t.len() == 22 {
            return NodeId(t.to_string());
        }
    }
    let id = NodeId::random();
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(dir));
    let _ = std::fs::write(&path, id.as_str());
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("bs-node-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = persisted_id(&dir);
        let b = persisted_id(&dir);
        assert_eq!(a, b);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
