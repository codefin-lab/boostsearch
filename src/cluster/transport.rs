//! The wire between nodes, as a dependency.
//!
//! A `Transport` carries `Envelope`s between `NodeId`s and says nothing
//! about how; the TCP one in `tcp.rs` is what production uses, the
//! simulation's is a queue a scheduler drains. Messages are framed the same
//! way on any transport: a length, a header naming the action and the
//! request it belongs to, and a body.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// A node's identity on the wire: 22 characters of base64url, as
/// OpenSearch names its nodes.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub String);

impl NodeId {
    /// A fresh random id: 16 random bytes, base64url without padding.
    pub fn random() -> NodeId {
        let mut bytes = [0u8; 16];
        // a system source, then hashed with a counter so that two ids drawn
        // in the same nanosecond still differ
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
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            use std::io::Read as _;
            let mut r = [0u8; 16];
            if f.read_exact(&mut r).is_ok() {
                h.update(r);
            }
        }
        bytes.copy_from_slice(&h.finalize()[..16]);
        use base64::Engine;
        NodeId(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.0)
    }
}

/// What kind of message an envelope carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Kind {
    Request,
    Response,
    /// a response that says the request failed, its body the reason
    Error,
}

/// One message on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub kind: Kind,
    /// pairs a response with its request; a request's own number
    pub request_id: u64,
    /// the action name, as OpenSearch names transport actions
    /// (`internal:cluster/coordination/join`, `indices:data/write/bulk[s]`)
    pub action: String,
    pub from: NodeId,
    /// the body, JSON: what the action carries
    pub body: Vec<u8>,
}

/// The framing version, first byte of every frame.
pub const FRAME_VERSION: u8 = 1;
/// The most a frame may carry: a shard's worth of bulk, not more.
pub const MAX_FRAME: usize = 512 * 1024 * 1024;

impl Envelope {
    pub fn request(action: &str, from: NodeId, request_id: u64, body: Vec<u8>) -> Envelope {
        Envelope { kind: Kind::Request, request_id, action: action.into(), from, body }
    }

    pub fn response(&self, from: NodeId, body: Vec<u8>) -> Envelope {
        Envelope {
            kind: Kind::Response,
            request_id: self.request_id,
            action: self.action.clone(),
            from,
            body,
        }
    }

    pub fn error(&self, from: NodeId, why: &str) -> Envelope {
        Envelope {
            kind: Kind::Error,
            request_id: self.request_id,
            action: self.action.clone(),
            from,
            body: why.as_bytes().to_vec(),
        }
    }

    pub fn body_json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }

    /// The frame: `u32 length | u8 version | u8 kind | u64 request id |
    /// u16 action length | action | u8 from length | from | body`. The
    /// length counts everything after itself.
    pub fn encode(&self) -> Vec<u8> {
        let action = self.action.as_bytes();
        let from = self.from.0.as_bytes();
        let len = 1 + 1 + 8 + 2 + action.len() + 1 + from.len() + self.body.len();
        let mut out = Vec::with_capacity(4 + len);
        out.extend_from_slice(&(len as u32).to_be_bytes());
        out.push(FRAME_VERSION);
        out.push(match self.kind {
            Kind::Request => 0,
            Kind::Response => 1,
            Kind::Error => 2,
        });
        out.extend_from_slice(&self.request_id.to_be_bytes());
        out.extend_from_slice(&(action.len() as u16).to_be_bytes());
        out.extend_from_slice(action);
        out.push(from.len() as u8);
        out.extend_from_slice(from);
        out.extend_from_slice(&self.body);
        out
    }

    /// One frame's payload (after its length), read back.
    pub fn decode(payload: &[u8]) -> Result<Envelope, FrameError> {
        if payload.len() < 12 {
            return Err(FrameError::Short);
        }
        if payload[0] != FRAME_VERSION {
            return Err(FrameError::Version(payload[0]));
        }
        let kind = match payload[1] {
            0 => Kind::Request,
            1 => Kind::Response,
            2 => Kind::Error,
            other => return Err(FrameError::Kind(other)),
        };
        let request_id = u64::from_be_bytes(payload[2..10].try_into().unwrap());
        let action_len = u16::from_be_bytes([payload[10], payload[11]]) as usize;
        let mut at = 12;
        let action =
            std::str::from_utf8(payload.get(at..at + action_len).ok_or(FrameError::Short)?)
                .map_err(|_| FrameError::Text)?
                .to_string();
        at += action_len;
        let from_len = *payload.get(at).ok_or(FrameError::Short)? as usize;
        at += 1;
        let from = std::str::from_utf8(payload.get(at..at + from_len).ok_or(FrameError::Short)?)
            .map_err(|_| FrameError::Text)?
            .to_string();
        at += from_len;
        Ok(Envelope { kind, request_id, action, from: NodeId(from), body: payload[at..].to_vec() })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    Short,
    Version(u8),
    Kind(u8),
    Text,
    TooLong(usize),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Short => write!(f, "frame too short"),
            FrameError::Version(v) => write!(f, "frame version {v} not understood"),
            FrameError::Kind(k) => write!(f, "frame kind {k} not understood"),
            FrameError::Text => write!(f, "frame names are not text"),
            FrameError::TooLong(n) => write!(f, "frame of {n} bytes is longer than allowed"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Where a message is going, and what happened to it.
#[derive(Debug)]
pub enum SendError {
    /// no way to reach that node right now
    Unreachable(NodeId),
    Closed,
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendError::Unreachable(n) => write!(f, "node {n} cannot be reached"),
            SendError::Closed => write!(f, "transport is closed"),
        }
    }
}

impl std::error::Error for SendError {}

/// What a node does with a message that arrived for it.
pub trait Handler: Send + Sync {
    fn handle(&self, envelope: Envelope);
}

/// The wire. A transport delivers envelopes to other nodes and hands the
/// ones that arrive to a handler; how is its own business.
pub trait Transport: Send + Sync {
    /// This node's own id.
    fn local(&self) -> NodeId;
    /// Send to a node. Delivery is not promised: a partition or a crash
    /// loses messages, and every protocol above assumes so.
    fn send(&self, to: &NodeId, envelope: Envelope) -> Result<(), SendError>;
    /// Where incoming messages go.
    fn set_handler(&self, handler: Arc<dyn Handler>);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let e = Envelope::request(
            "internal:cluster/coordination/join",
            NodeId("abc".into()),
            42,
            b"{\"x\":1}".to_vec(),
        );
        let bytes = e.encode();
        let len = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(len, bytes.len() - 4);
        let back = Envelope::decode(&bytes[4..]).unwrap();
        assert_eq!(back.kind, Kind::Request);
        assert_eq!(back.request_id, 42);
        assert_eq!(back.action, "internal:cluster/coordination/join");
        assert_eq!(back.from, NodeId("abc".into()));
        assert_eq!(back.body, b"{\"x\":1}");
    }

    #[test]
    fn ids_look_like_opensearch_ids() {
        let a = NodeId::random();
        let b = NodeId::random();
        assert_eq!(a.0.len(), 22);
        assert_ne!(a, b);
        assert!(a.0.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn refuses_other_versions() {
        let mut bytes = Envelope::request("a", NodeId("n".into()), 1, vec![]).encode();
        bytes[4] = 9;
        assert_eq!(Envelope::decode(&bytes[4..]), Err(FrameError::Version(9)));
    }
}
