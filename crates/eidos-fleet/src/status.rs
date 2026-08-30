//! Status surfaces: what `eidos fleet status` and `/api/fleet` show.
//! Identifies connection direction, peer, per-source cursors, backlog,
//! freshness, last error, and any fence or repair state (sprint section
//! 11, "operability").

use crate::metrics::FleetCountersView;
use eidos_catalog::fleet::NodeId;
use eidos_domain::{SourceId, UnixNanos};
use serde::Serialize;
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// This side dialed.
    Outbound,
    /// The peer dialed.
    Inbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SyncRole {
    /// This side ships the source.
    Shipping,
    /// This side applies the source.
    Consuming,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct SessionSourceView {
    /// The source id on this side (a local source when shipping, the
    /// replica's local id when consuming).
    pub source_id: SourceId,
    pub role: SyncRole,
    /// Shipper phase or consumer state, in words.
    pub phase: String,
    /// Sequence the peer has durably acknowledged (shipping) or that this
    /// side has durably applied (consuming).
    pub cursor: u64,
    pub head: u64,
    pub in_flight_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub last_error: Option<String>,
    pub batches: u64,
    pub rows: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct SessionView {
    pub peer: NodeId,
    pub peer_name: String,
    pub direction: Direction,
    pub since: UnixNanos,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub remote_addr: Option<String>,
    pub last_activity_ms_ago: u64,
    /// Bytes this side may still put in flight towards the peer.
    pub credit_remaining: i64,
    pub sources: Vec<SessionSourceView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct PeerView {
    pub node_id: NodeId,
    pub name: String,
    pub role: String,
    pub fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub endpoint: Option<String>,
    pub enabled: bool,
    pub connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub last_seen_at: Option<UnixNanos>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub last_error: Option<String>,
    /// Reconnect state when this side dials the peer.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub next_dial_in_ms: Option<u64>,
}

/// A local source's ledger as seen by the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct LocalSourceSync {
    pub source_id: SourceId,
    pub name: String,
    pub policy: String,
    pub enabled: bool,
    pub ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub epoch: Option<String>,
    pub head_seq: u64,
    pub compacted_through: u64,
    pub backlog_rows: u64,
    pub backlog_tombstones: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub backlog_oldest_age_ms: Option<u64>,
    pub degraded: bool,
}

/// A replicated source as seen by the central.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct ReplicaSourceSync {
    pub source_id: SourceId,
    pub name: String,
    pub node: NodeId,
    pub node_name: String,
    pub remote_source_id: SourceId,
    pub epoch: String,
    pub applied_seq: u64,
    pub reported_head: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub applied_at: Option<UnixNanos>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub reported_at: Option<UnixNanos>,
    pub resyncing: bool,
    pub connected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct FleetStatus {
    pub node_id: NodeId,
    pub name: String,
    pub fingerprint: String,
    pub central: bool,
    /// Whether a central is configured for this node, and whether sync to
    /// it is enabled.
    pub enrolled: bool,
    pub sync_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub listen: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
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
    pub pending_invites: u64,
}
