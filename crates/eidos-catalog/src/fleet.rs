//! Fleet roster: the peers this installation trusts and the single-use
//! invitations a central mints for enrollment (ADR-0023).
//!
//! Identity lives outside the catalog (the node's key pair and certificate
//! are files in the data directory); the catalog records which peer
//! fingerprints are admitted and what they may do. Only the hash of an
//! invitation secret is stored.

use crate::{Catalog, CatalogError, Result};
use eidos_domain::{SourceId, SyncPolicy, UnixNanos};
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetInvite {
    pub token_hash: [u8; 32],
    pub name_hint: Option<String>,
    pub created_at: UnixNanos,
    pub expires_at: UnixNanos,
    pub used_at: Option<UnixNanos>,
    pub used_by: Option<NodeId>,
}

fn blob32(blob: Vec<u8>, what: &str) -> Result<[u8; 32]> {
    blob.try_into()
        .map_err(|_| CatalogError::InvalidState(format!("{what} is not 32 bytes")))
}

fn blob16(blob: Vec<u8>, what: &str) -> Result<[u8; 16]> {
    blob.try_into()
        .map_err(|_| CatalogError::InvalidState(format!("{what} is not 16 bytes")))
}

/// `(node_id, name, role, fingerprint, endpoint, enabled, enrolled_at, last_seen_at, last_error)`
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
    ))
}

fn peer_build(row: PeerRow) -> Result<FleetPeer> {
    let (id, name, role, fp, endpoint, enabled, enrolled, seen, error) = row;
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
    })
}

const PEER_COLUMNS: &str =
    "node_id, name, role, fingerprint, endpoint, enabled, enrolled_at, last_seen_at, last_error";

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

    /// Record a minted invitation by the hash of its secret.
    pub fn fleet_add_invite(
        &self,
        token_hash: [u8; 32],
        name_hint: Option<&str>,
        expires_at: UnixNanos,
    ) -> Result<()> {
        self.with_writer(|conn| {
            conn.execute(
                "INSERT INTO fleet_invites (token_hash, name_hint, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    token_hash.as_slice(),
                    name_hint,
                    UnixNanos::now().0,
                    expires_at.0
                ],
            )?;
            Ok(())
        })
    }

    /// Consume an invitation: valid exactly once, before it expires. Returns
    /// the invite when it was redeemed now.
    pub fn fleet_redeem_invite(
        &self,
        token_hash: [u8; 32],
        node: NodeId,
    ) -> Result<Option<FleetInvite>> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let now = UnixNanos::now().0;
            let n = tx.execute(
                "UPDATE fleet_invites SET used_at = ?2, used_by = ?3
                 WHERE token_hash = ?1 AND used_at IS NULL AND expires_at > ?2",
                params![token_hash.as_slice(), now, node.0.as_slice()],
            )?;
            let invite = if n == 0 {
                None
            } else {
                tx.query_row(
                    "SELECT token_hash, name_hint, created_at, expires_at, used_at, used_by
                     FROM fleet_invites WHERE token_hash = ?1",
                    params![token_hash.as_slice()],
                    |r| {
                        Ok((
                            r.get::<_, Vec<u8>>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, Option<i64>>(4)?,
                            r.get::<_, Option<Vec<u8>>>(5)?,
                        ))
                    },
                )
                .optional()?
                .map(|(hash, hint, created, expires, used, by)| {
                    Ok::<_, CatalogError>(FleetInvite {
                        token_hash: blob32(hash, "invite hash")?,
                        name_hint: hint,
                        created_at: UnixNanos(created),
                        expires_at: UnixNanos(expires),
                        used_at: used.map(UnixNanos),
                        used_by: by
                            .map(|b| blob16(b, "invite node id").map(NodeId))
                            .transpose()?,
                    })
                })
                .transpose()?
            };
            tx.commit()?;
            Ok(invite)
        })
    }

    /// Consume an invitation and admit its peer in the same transaction.
    /// A crash or roster write failure cannot burn a single-use invitation
    /// without creating the corresponding roster entry.
    pub fn fleet_redeem_invite_and_upsert_peer(
        &self,
        token_hash: [u8; 32],
        peer: &FleetPeer,
    ) -> Result<Option<FleetInvite>> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let now = UnixNanos::now().0;
            let n = tx.execute(
                "UPDATE fleet_invites SET used_at = ?2, used_by = ?3
                 WHERE token_hash = ?1 AND used_at IS NULL AND expires_at > ?2",
                params![token_hash.as_slice(), now, peer.node_id.0.as_slice()],
            )?;
            let invite = if n == 0 {
                None
            } else {
                let invite = tx.query_row(
                    "SELECT token_hash, name_hint, created_at, expires_at, used_at, used_by
                     FROM fleet_invites WHERE token_hash = ?1",
                    params![token_hash.as_slice()],
                    |r| {
                        Ok((
                            r.get::<_, Vec<u8>>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, Option<i64>>(4)?,
                            r.get::<_, Option<Vec<u8>>>(5)?,
                        ))
                    },
                )?;
                let invite = FleetInvite {
                    token_hash: blob32(invite.0, "invite hash")?,
                    name_hint: invite.1,
                    created_at: UnixNanos(invite.2),
                    expires_at: UnixNanos(invite.3),
                    used_at: invite.4.map(UnixNanos),
                    used_by: invite
                        .5
                        .map(|b| blob16(b, "invite node id").map(NodeId))
                        .transpose()?,
                };
                let mut enrolled = peer.clone();
                if let Some(hint) = invite.name_hint.as_deref().filter(|hint| !hint.is_empty()) {
                    enrolled.name = hint.to_string();
                }
                upsert_peer_conn(&tx, &enrolled)?;
                Some(invite)
            };
            tx.commit()?;
            Ok(invite)
        })
    }

    /// Drop expired and redeemed invitations.
    pub fn fleet_prune_invites(&self) -> Result<u64> {
        self.with_writer(|conn| {
            Ok(conn.execute(
                "DELETE FROM fleet_invites WHERE used_at IS NOT NULL OR expires_at <= ?1",
                params![UnixNanos::now().0],
            )? as u64)
        })
    }

    pub fn fleet_pending_invites(&self) -> Result<u64> {
        self.with_reader(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM fleet_invites WHERE used_at IS NULL AND expires_at > ?1",
                params![UnixNanos::now().0],
                |r| r.get::<_, i64>(0),
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
