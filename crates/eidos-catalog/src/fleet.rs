//! Fleet roster and approval queue (ADR-0023).
//!
//! Identity lives outside the catalog (the node's key pair and certificate
//! are files in the data directory); the catalog records which peer
//! fingerprints are admitted and what they may do. Unknown nodes may leave a
//! durable join request, but cannot replicate until the designated master
//! approves that exact certificate identity.

use crate::{Catalog, CatalogError, Result};
use eidos_domain::{HostId, SourceId, SourceState, SyncPolicy, UnixNanos};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::fmt;
use ts_rs::TS;

/// Stable 16-byte fleet node identity, derived from the node's certificate
/// public key. Serializes as 32 hex characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, TS)]
#[ts(type = "string")]
pub struct NodeId(pub [u8; 16]);

impl Serialize for NodeId {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse_hex(&s)
            .ok_or_else(|| serde::de::Error::custom("node id is not 32 hex characters"))
    }
}

impl NodeId {
    pub fn to_hex(self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn parse_hex(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.len() != 32 {
            return None;
        }
        let hex = |byte| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        };
        let bytes = s.as_bytes();
        let mut out = [0u8; 16];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = (hex(bytes[i * 2])? << 4) | hex(bytes[i * 2 + 1])?;
        }
        Some(Self(out))
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.to_hex())
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// One row of the catalog's host table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct HostRecord {
    pub id: HostId,
    pub name: String,
    pub platform: String,
}

/// What a peer is to this installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerRole {
    /// An enrolled node whose sources this central replicates.
    Node,
    /// The central this node replicates into.
    Central,
}

impl PeerRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Central => "central",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "node" => Some(Self::Node),
            "central" => Some(Self::Central),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetPeer {
    pub node_id: NodeId,
    pub name: String,
    pub role: PeerRole,
    /// SHA-256 of the peer's certificate public key (DER SPKI).
    pub fingerprint: [u8; 32],
    /// Where to dial the peer, when this side initiates.
    pub endpoint: Option<String>,
    pub enabled: bool,
    pub enrolled_at: UnixNanos,
    pub last_seen_at: Option<UnixNanos>,
    pub last_error: Option<String>,
    /// A sync session with the peer is open right now (reset when the
    /// service starts).
    #[serde(default)]
    pub connected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinRequestStatus {
    Pending,
    Approved,
    Rejected,
}

impl JoinRequestStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "approved" => Some(Self::Approved),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetJoinRequest {
    /// Random 128-bit attempt identity encoded as lowercase hex. A rejected
    /// attempt stays rejected; retrying creates a fresh id and a new explicit
    /// approval decision.
    pub request_id: String,
    pub node_id: NodeId,
    pub name: String,
    pub platform: String,
    pub fingerprint: [u8; 32],
    pub remote_addr: Option<String>,
    pub requested_at: UnixNanos,
    pub last_seen_at: UnixNanos,
    pub status: JoinRequestStatus,
    pub decided_at: Option<UnixNanos>,
}

fn blob32(blob: Vec<u8>, what: &str) -> Result<[u8; 32]> {
    blob.try_into()
        .map_err(|_| CatalogError::InvalidState(format!("{what} is not 32 bytes")))
}

fn blob16(blob: Vec<u8>, what: &str) -> Result<[u8; 16]> {
    blob.try_into()
        .map_err(|_| CatalogError::InvalidState(format!("{what} is not 16 bytes")))
}

/// `(node_id, name, role, fingerprint, endpoint, enabled, enrolled_at, last_seen_at, last_error, connected)`
type PeerRow = (
    Vec<u8>,
    String,
    String,
    Vec<u8>,
    Option<String>,
    i64,
    i64,
    Option<i64>,
    Option<String>,
    i64,
);

fn peer_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PeerRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
    ))
}

fn peer_build(row: PeerRow) -> Result<FleetPeer> {
    let (id, name, role, fp, endpoint, enabled, enrolled, seen, error, connected) = row;
    Ok(FleetPeer {
        node_id: NodeId(blob16(id, "fleet node id")?),
        name,
        role: PeerRole::parse(&role)
            .ok_or_else(|| CatalogError::InvalidState(format!("unknown peer role {role}")))?,
        fingerprint: blob32(fp, "peer fingerprint")?,
        endpoint,
        enabled: enabled != 0,
        enrolled_at: UnixNanos(enrolled),
        last_seen_at: seen.map(UnixNanos),
        last_error: error,
        connected: connected != 0,
    })
}

const PEER_COLUMNS: &str = "node_id, name, role, fingerprint, endpoint, enabled, enrolled_at, last_seen_at, last_error, connected";
const JOIN_REQUEST_COLUMNS: &str = "request_id, node_id, name, platform, fingerprint, remote_addr, requested_at, last_seen_at, status, decided_at";

type JoinRequestRow = (
    String,
    Vec<u8>,
    String,
    String,
    Vec<u8>,
    Option<String>,
    i64,
    i64,
    String,
    Option<i64>,
);

fn canonical_request_id(value: &str) -> Result<String> {
    NodeId::parse_hex(value).map(NodeId::to_hex).ok_or_else(|| {
        CatalogError::InvalidState("join request id is not 32 hex characters".into())
    })
}

fn join_request_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<JoinRequestRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
    ))
}

fn join_request_build(row: JoinRequestRow) -> Result<FleetJoinRequest> {
    Ok(FleetJoinRequest {
        request_id: canonical_request_id(&row.0)?,
        node_id: NodeId(blob16(row.1, "join request node id")?),
        name: row.2,
        platform: row.3,
        fingerprint: blob32(row.4, "join request fingerprint")?,
        remote_addr: row.5,
        requested_at: UnixNanos(row.6),
        last_seen_at: UnixNanos(row.7),
        status: JoinRequestStatus::parse(&row.8).ok_or_else(|| {
            CatalogError::InvalidState(format!("unknown join request status {}", row.8))
        })?,
        decided_at: row.9.map(UnixNanos),
    })
}

fn join_request_conn(conn: &Connection, request_id: &str) -> Result<Option<FleetJoinRequest>> {
    conn.prepare_cached(&format!(
        "SELECT {JOIN_REQUEST_COLUMNS} FROM fleet_join_requests WHERE request_id = ?1"
    ))?
    .query_row(params![request_id], join_request_from_row)
    .optional()?
    .map(join_request_build)
    .transpose()
}

pub(crate) fn peer_conn(conn: &Connection, node: NodeId) -> Result<Option<FleetPeer>> {
    conn.prepare_cached(&format!(
        "SELECT {PEER_COLUMNS} FROM fleet_peers WHERE node_id = ?1"
    ))?
    .query_row(params![node.0.as_slice()], peer_from_row)
    .optional()?
    .map(peer_build)
    .transpose()
}

fn upsert_peer_conn(conn: &Connection, peer: &FleetPeer) -> Result<()> {
    if let Some(existing) = peer_conn(conn, peer.node_id)? {
        if existing.fingerprint != peer.fingerprint {
            return Err(CatalogError::InvalidState(format!(
                "peer {} is already enrolled with a different key",
                peer.node_id
            )));
        }
    }
    conn.execute(
        "INSERT INTO fleet_peers (node_id, name, role, fingerprint, endpoint, enabled, enrolled_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(node_id) DO UPDATE SET name = excluded.name, role = excluded.role,
            endpoint = excluded.endpoint, enabled = excluded.enabled",
        params![
            peer.node_id.0.as_slice(),
            peer.name,
            peer.role.as_str(),
            peer.fingerprint.as_slice(),
            peer.endpoint,
            peer.enabled as i64,
            peer.enrolled_at.0
        ],
    )?;
    Ok(())
}

impl Catalog {
    /// Admit (or update) a peer. The fingerprint is the trust anchor; a
    /// changed fingerprint for a known node id is refused, because node ids
    /// are derived from keys and a different key is a different node.
    pub fn fleet_upsert_peer(&self, peer: &FleetPeer) -> Result<()> {
        self.with_writer(|conn| upsert_peer_conn(conn, peer))
    }

    pub fn fleet_peer(&self, node: NodeId) -> Result<Option<FleetPeer>> {
        self.with_reader(|conn| peer_conn(conn, node))
    }

    /// The peer whose certificate public key hashes to `fingerprint`.
    pub fn fleet_peer_by_fingerprint(&self, fingerprint: &[u8; 32]) -> Result<Option<FleetPeer>> {
        self.with_reader(|conn| {
            conn.prepare_cached(&format!(
                "SELECT {PEER_COLUMNS} FROM fleet_peers WHERE fingerprint = ?1"
            ))?
            .query_row(params![fingerprint.as_slice()], peer_from_row)
            .optional()?
            .map(peer_build)
            .transpose()
        })
    }

    pub fn fleet_peers(&self) -> Result<Vec<FleetPeer>> {
        self.with_reader(|conn| {
            conn.prepare_cached(&format!(
                "SELECT {PEER_COLUMNS} FROM fleet_peers ORDER BY name"
            ))?
            .query_map([], peer_from_row)?
            .map(|row| peer_build(row?))
            .collect()
        })
    }

    pub fn fleet_set_peer_endpoint(&self, node: NodeId, endpoint: Option<&str>) -> Result<bool> {
        self.with_writer(|conn| {
            Ok(conn.execute(
                "UPDATE fleet_peers SET endpoint = ?2 WHERE node_id = ?1",
                params![node.0.as_slice(), endpoint],
            )? > 0)
        })
    }

    pub fn fleet_set_peer_enabled(&self, node: NodeId, enabled: bool) -> Result<bool> {
        self.with_writer(|conn| {
            Ok(conn.execute(
                "UPDATE fleet_peers SET enabled = ?2 WHERE node_id = ?1",
                params![node.0.as_slice(), enabled as i64],
            )? > 0)
        })
    }

    /// Record whether a session with the peer is open. Sessions are RAM;
    /// [`Catalog::fleet_reset_connected`] clears every flag at service start.
    pub fn fleet_set_peer_connected(&self, node: NodeId, connected: bool) -> Result<()> {
        self.with_writer(|conn| {
            conn.execute(
                "UPDATE fleet_peers SET connected = ?2 WHERE node_id = ?1",
                params![node.0.as_slice(), connected as i64],
            )?;
            Ok(())
        })
    }

    pub fn fleet_reset_connected(&self) -> Result<()> {
        self.with_writer(|conn| {
            conn.execute("UPDATE fleet_peers SET connected = 0", [])?;
            Ok(())
        })
    }

    /// Hosts known to the catalog (this one and every enrolled origin).
    pub fn list_hosts(&self) -> Result<Vec<HostRecord>> {
        self.with_reader(|conn| {
            conn.prepare_cached("SELECT host_id, name, platform FROM hosts ORDER BY host_id")?
                .query_map([], |r| {
                    Ok(HostRecord {
                        id: HostId(r.get(0)?),
                        name: r.get(1)?,
                        platform: r.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()
                .map_err(Into::into)
        })
    }

    pub fn fleet_note_peer_seen(&self, node: NodeId, error: Option<&str>) -> Result<()> {
        self.with_writer(|conn| {
            conn.execute(
                "UPDATE fleet_peers SET last_seen_at = CASE WHEN ?2 IS NULL THEN ?3 ELSE last_seen_at END,
                    last_error = ?2 WHERE node_id = ?1",
                params![node.0.as_slice(), error, UnixNanos::now().0],
            )?;
            Ok(())
        })
    }

    /// Forget a peer. Its replicated sources are retired by the caller
    /// (see [`Catalog::replica_retire_source`]).
    pub fn fleet_remove_peer(&self, node: NodeId) -> Result<bool> {
        self.with_writer(|conn| {
            Ok(conn.execute(
                "DELETE FROM fleet_peers WHERE node_id = ?1",
                params![node.0.as_slice()],
            )? > 0)
        })
    }

    /// Atomically revoke a peer and hide every replica it owns. Physical row
    /// cleanup is deliberately separate and idempotent, but no replica write
    /// or search result can cross this transaction boundary.
    pub fn fleet_forget_peer(&self, node: NodeId) -> Result<Option<Vec<SourceId>>> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let sources: Vec<SourceId> = tx
                .prepare(
                    "SELECT source_id FROM sync_replica_sources
                     WHERE node_id = ?1 ORDER BY source_id",
                )?
                .query_map(params![node.0.as_slice()], |row| {
                    row.get::<_, i64>(0).map(SourceId)
                })?
                .collect::<rusqlite::Result<_>>()?;
            let existed = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM fleet_peers WHERE node_id = ?1)",
                params![node.0.as_slice()],
                |row| row.get::<_, i64>(0),
            )? != 0;
            if !existed && sources.is_empty() {
                tx.commit()?;
                return Ok(None);
            }
            let now = UnixNanos::now().0;
            tx.execute(
                "UPDATE sources SET state = ?2,
                     state_reason = 'retired from the fleet', updated_at = ?3
                 WHERE source_id IN (
                     SELECT source_id FROM sync_replica_sources WHERE node_id = ?1)",
                params![node.0.as_slice(), SourceState::Retired.as_str(), now],
            )?;
            tx.execute(
                "DELETE FROM sync_replica_repairs WHERE source_id IN (
                     SELECT source_id FROM sync_replica_sources WHERE node_id = ?1)",
                params![node.0.as_slice()],
            )?;
            tx.execute(
                "DELETE FROM fleet_peers WHERE node_id = ?1",
                params![node.0.as_slice()],
            )?;
            tx.commit()?;
            Ok(Some(sources))
        })
    }

    /// Record or refresh a join request from the same proved certificate.
    /// Decisions are never reset by a replay; a retry after rejection uses a
    /// fresh request id and therefore requires a fresh operator decision.
    pub fn fleet_record_join_request(
        &self,
        request: &FleetJoinRequest,
    ) -> Result<FleetJoinRequest> {
        let request_id = canonical_request_id(&request.request_id)?;
        self.with_writer(|conn| {
            if let Some(existing) = join_request_conn(conn, &request_id)? {
                if existing.node_id != request.node_id
                    || existing.fingerprint != request.fingerprint
                {
                    return Err(CatalogError::InvalidState(
                        "join request id was replayed by a different certificate".into(),
                    ));
                }
                conn.execute(
                    "UPDATE fleet_join_requests SET name = ?2, platform = ?3,
                         remote_addr = ?4, last_seen_at = ?5 WHERE request_id = ?1",
                    params![
                        request_id,
                        request.name,
                        request.platform,
                        request.remote_addr,
                        UnixNanos::now().0
                    ],
                )?;
            } else {
                conn.execute(
                    "INSERT INTO fleet_join_requests
                         (request_id, node_id, name, platform, fingerprint, remote_addr,
                          requested_at, last_seen_at, status)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending')",
                    params![
                        request_id,
                        request.node_id.0.as_slice(),
                        request.name,
                        request.platform,
                        request.fingerprint.as_slice(),
                        request.remote_addr,
                        request.requested_at.0,
                        request.last_seen_at.0,
                    ],
                )?;
            }
            join_request_conn(conn, &request_id)?.ok_or_else(|| {
                CatalogError::InvalidState("recorded join request disappeared".into())
            })
        })
    }

    pub fn fleet_join_request(&self, request_id: &str) -> Result<Option<FleetJoinRequest>> {
        let request_id = canonical_request_id(request_id)?;
        self.with_reader(|conn| join_request_conn(conn, &request_id))
    }

    pub fn fleet_pending_join_requests(&self) -> Result<Vec<FleetJoinRequest>> {
        self.with_reader(|conn| {
            conn.prepare_cached(&format!(
                "SELECT {JOIN_REQUEST_COLUMNS} FROM fleet_join_requests
                 WHERE status = 'pending' ORDER BY requested_at"
            ))?
            .query_map([], join_request_from_row)?
            .map(|row| join_request_build(row?))
            .collect()
        })
    }

    /// Approve or reject a pending attempt. Approval and roster admission
    /// commit atomically, so a lost response is completed by the node's next
    /// poll without requiring another click.
    pub fn fleet_decide_join_request(
        &self,
        request_id: &str,
        approve: bool,
    ) -> Result<Option<FleetJoinRequest>> {
        let request_id = canonical_request_id(request_id)?;
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let Some(mut request) = join_request_conn(&tx, &request_id)? else {
                tx.commit()?;
                return Ok(None);
            };
            let wanted = if approve {
                JoinRequestStatus::Approved
            } else {
                JoinRequestStatus::Rejected
            };
            if request.status != JoinRequestStatus::Pending && request.status != wanted {
                return Err(CatalogError::InvalidState(format!(
                    "join request is already {}",
                    request.status.as_str()
                )));
            }
            if approve {
                upsert_peer_conn(
                    &tx,
                    &FleetPeer {
                        node_id: request.node_id,
                        name: request.name.clone(),
                        role: PeerRole::Node,
                        fingerprint: request.fingerprint,
                        endpoint: None,
                        enabled: true,
                        enrolled_at: UnixNanos::now(),
                        last_seen_at: Some(request.last_seen_at),
                        last_error: None,
                        connected: false,
                    },
                )?;
            }
            let now = UnixNanos::now();
            tx.execute(
                "UPDATE fleet_join_requests SET status = ?2, decided_at = COALESCE(decided_at, ?3)
                 WHERE request_id = ?1",
                params![request_id, wanted.as_str(), now.0],
            )?;
            tx.commit()?;
            request.status = wanted;
            request.decided_at.get_or_insert(now);
            Ok(Some(request))
        })
    }

    /// Bound abandoned approval attempts. Active nodes refresh `last_seen_at`
    /// on every poll, so only requests silent for thirty days are removed.
    pub fn fleet_prune_join_requests(&self) -> Result<u64> {
        const RETENTION_NANOS: i64 = 30 * 24 * 60 * 60 * 1_000_000_000;
        self.with_writer(|conn| {
            Ok(conn.execute(
                "DELETE FROM fleet_join_requests WHERE last_seen_at <= ?1",
                params![UnixNanos::now().0.saturating_sub(RETENTION_NANOS)],
            )? as u64)
        })
    }

    /// Whether a source takes part in replication once its node is
    /// enrolled. Remote sources cannot be given a policy: they are never
    /// shipped from here.
    pub fn set_sync_policy(&self, source: SourceId, policy: SyncPolicy) -> Result<()> {
        self.with_writer(|conn| {
            let kind: Option<String> = conn
                .query_row(
                    "SELECT kind FROM sources WHERE source_id = ?1",
                    params![source.0],
                    |r| r.get(0),
                )
                .optional()?;
            match kind.as_deref() {
                None => return Err(CatalogError::NotFound(format!("source {source}"))),
                Some("remote") => {
                    return Err(CatalogError::InvalidState(
                        "a replicated source has no sync policy of its own".into(),
                    ))
                }
                Some(_) => {}
            }
            conn.execute(
                "UPDATE sources SET sync_policy = ?2, updated_at = ?3 WHERE source_id = ?1",
                params![source.0, policy.as_str(), UnixNanos::now().0],
            )?;
            Ok(())
        })
    }
}
