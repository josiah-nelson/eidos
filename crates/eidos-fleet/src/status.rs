//! Status surfaces: what `eidos fleet status` and `/api/fleet` show.
//! Identifies connection direction, peer, per-source cursors, backlog,
//! freshness, last error, and any fence or repair state (sprint section
//! 11, "operability").

use crate::metrics::FleetCountersView;
use eidos_catalog::fleet::NodeId;
use eidos_domain::{SourceId, UnixNanos};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// This side dialed.
    Outbound,
    /// The peer dialed.
    Inbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SyncRole {
    /// This side ships the source.
    Shipping,
    /// This side applies the source.
    Consuming,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SessionSourceView {
    /// The source id on this side (a local source when shipping, the
    /// replica's local id when consuming).
    pub source_id: SourceId,
    pub role: SyncRole,
    /// Shipper phase or consumer state, in words.
    pub phase: String,
    /// Sequence the peer has durably acknowledged (shipping) or that this
    /// side has durably applied (consuming).
    #[serde(deserialize_with = "eidos_domain::json::u64_string::deserialize")]
    pub cursor: u64,
    #[serde(deserialize_with = "eidos_domain::json::u64_string::deserialize")]
    pub head: u64,
    #[serde(deserialize_with = "eidos_domain::json::u64_string::deserialize")]
    pub in_flight_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub last_error: Option<String>,
    #[serde(deserialize_with = "eidos_domain::json::u64_string::deserialize")]
    pub batches: u64,
    #[serde(deserialize_with = "eidos_domain::json::u64_string::deserialize")]
    pub rows: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SessionView {
    pub peer: NodeId,
    pub peer_name: String,
    pub direction: Direction,
    pub since: UnixNanos,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub remote_addr: Option<String>,
    #[serde(deserialize_with = "eidos_domain::json::u64_string::deserialize")]
    pub last_activity_ms_ago: u64,
    /// Bytes this side may still put in flight towards the peer.
    #[serde(deserialize_with = "eidos_domain::json::i64_string::deserialize")]
    pub credit_remaining: i64,
    pub sources: Vec<SessionSourceView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PeerView {
    pub node_id: NodeId,
    pub name: String,
    pub role: String,
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub endpoint: Option<String>,
    pub enabled: bool,
    pub connected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub last_seen_at: Option<UnixNanos>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub last_error: Option<String>,
    /// Reconnect state when this side dials the peer.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "eidos_domain::json::option_u64_string::deserialize"
    )]
    #[ts(optional)]
    pub next_dial_in_ms: Option<u64>,
}

/// A local source's ledger as seen by the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LocalSourceSync {
    pub source_id: SourceId,
    pub name: String,
    pub policy: String,
    pub enabled: bool,
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub epoch: Option<String>,
    #[serde(deserialize_with = "eidos_domain::json::u64_string::deserialize")]
    pub head_seq: u64,
    #[serde(deserialize_with = "eidos_domain::json::u64_string::deserialize")]
    pub compacted_through: u64,
    #[serde(deserialize_with = "eidos_domain::json::u64_string::deserialize")]
    pub backlog_rows: u64,
    #[serde(deserialize_with = "eidos_domain::json::u64_string::deserialize")]
    pub backlog_tombstones: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "eidos_domain::json::option_u64_string::deserialize"
    )]
    #[ts(optional)]
    pub backlog_oldest_age_ms: Option<u64>,
    pub degraded: bool,
}

/// A replicated source as seen by the central.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ReplicaSourceSync {
    pub source_id: SourceId,
    pub name: String,
    pub node: NodeId,
    pub node_name: String,
    pub remote_source_id: SourceId,
    pub epoch: String,
    #[serde(deserialize_with = "eidos_domain::json::u64_string::deserialize")]
    pub applied_seq: u64,
    #[serde(deserialize_with = "eidos_domain::json::u64_string::deserialize")]
    pub reported_head: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub applied_at: Option<UnixNanos>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub reported_at: Option<UnixNanos>,
    pub resyncing: bool,
    pub connected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct FleetStatus {
    pub node_id: NodeId,
    pub name: String,
    pub fingerprint: String,
    pub central: bool,
    /// Whether a central is configured for this node, and whether sync to
    /// it is enabled.
    pub enrolled: bool,
    pub sync_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub listen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub listening: Option<String>,
    pub peers: Vec<PeerView>,
    pub sessions: Vec<SessionView>,
    pub local_sources: Vec<LocalSourceSync>,
    pub replica_sources: Vec<ReplicaSourceSync>,
    pub counters: FleetCountersView,
    /// Conditions an operator should see: backlog over its ceiling, a
    /// fenced source, a listener that failed to bind.
    pub degraded: Vec<String>,
    #[serde(deserialize_with = "eidos_domain::json::u64_string::deserialize")]
    pub pending_invites: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn stringify_numbers(value: &mut Value) {
        match value {
            Value::Number(number) => *value = Value::String(number.to_string()),
            Value::Array(values) => values.iter_mut().for_each(stringify_numbers),
            Value::Object(values) => values.values_mut().for_each(stringify_numbers),
            _ => {}
        }
    }

    #[test]
    fn api_decimal_strings_deserialize_through_the_complete_status_shape() {
        let node = NodeId([1; 16]);
        let status = FleetStatus {
            node_id: node,
            name: "node".into(),
            fingerprint: "fingerprint".into(),
            central: true,
            enrolled: false,
            sync_enabled: true,
            listen: Some("0.0.0.0:7701".into()),
            listening: Some("0.0.0.0:7701".into()),
            peers: vec![PeerView {
                node_id: node,
                name: "peer".into(),
                role: "node".into(),
                fingerprint: "peer-fingerprint".into(),
                endpoint: Some("127.0.0.1:7701".into()),
                enabled: true,
                connected: false,
                last_seen_at: Some(UnixNanos(123)),
                last_error: None,
                next_dial_in_ms: Some(4),
            }],
            sessions: vec![SessionView {
                peer: node,
                peer_name: "peer".into(),
                direction: Direction::Inbound,
                since: UnixNanos(456),
                remote_addr: Some("127.0.0.1:1234".into()),
                last_activity_ms_ago: 5,
                credit_remaining: -6,
                sources: vec![SessionSourceView {
                    source_id: SourceId(7),
                    role: SyncRole::Shipping,
                    phase: "streaming".into(),
                    cursor: 8,
                    head: 9,
                    in_flight_bytes: 10,
                    last_error: None,
                    batches: 11,
                    rows: 12,
                }],
            }],
            local_sources: vec![LocalSourceSync {
                source_id: SourceId(13),
                name: "local".into(),
                policy: "metadata".into(),
                enabled: true,
                ready: true,
                epoch: Some("epoch".into()),
                head_seq: 14,
                compacted_through: 15,
                backlog_rows: 16,
                backlog_tombstones: 17,
                backlog_oldest_age_ms: Some(18),
                degraded: false,
            }],
            replica_sources: vec![ReplicaSourceSync {
                source_id: SourceId(19),
                name: "replica".into(),
                node,
                node_name: "node".into(),
                remote_source_id: SourceId(20),
                epoch: "epoch".into(),
                applied_seq: 21,
                reported_head: 22,
                applied_at: Some(UnixNanos(789)),
                reported_at: Some(UnixNanos(790)),
                resyncing: false,
                connected: true,
            }],
            counters: FleetCountersView {
                connections_attempted: 23,
                bytes_catalog_received: 24,
                ..Default::default()
            },
            degraded: vec![],
            pending_invites: 25,
        };
        let mut wire = serde_json::to_value(&status).unwrap();
        stringify_numbers(&mut wire);
        let decoded: FleetStatus = serde_json::from_value(wire).unwrap();
        assert_eq!(decoded, status);
    }
}
