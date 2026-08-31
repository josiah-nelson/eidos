//! Approval-based fleet joining.
//!
//! A node reaches a master by an address entered by the operator or learned
//! through local discovery. The first mutually authenticated TLS connection
//! observes the master's certificate (TOFU); every retry is pinned to it.
//! The node cannot start a sync session until the master approves the exact
//! certificate identity on its Nodes page.

use crate::config::{PendingJoinTarget, DEFAULT_SYNC_PORT};
use crate::identity::{hex, node_id_of, unhex, NodeIdentity};
use crate::tls;
use crate::wire::{self, Message};
use anyhow::{anyhow, Context};
use eidos_catalog::fleet::{FleetPeer, NodeId, PeerRole};
use eidos_catalog::Catalog;
use eidos_domain::UnixNanos;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

/// What an approved join established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinedFleet {
    pub master: NodeId,
    pub master_name: String,
    pub endpoint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinOutcome {
    Pending(PendingJoinTarget),
    Joined(JoinedFleet),
    Rejected(String),
}

/// Add the fleet port when an operator supplies only an IP address or host.
pub fn normalize_master_endpoint(value: &str) -> anyhow::Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(anyhow!("enter the master's IP address or host name"));
    }
    if value.contains("://") || value.contains('/') || value.chars().any(char::is_whitespace) {
        return Err(anyhow!(
            "master address must be an IP or host name, with an optional port"
        ));
    }
    let unbracketed = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(value);
    if let Ok(ip) = unbracketed.parse::<IpAddr>() {
        return Ok(match ip {
            IpAddr::V4(_) => format!("{ip}:{DEFAULT_SYNC_PORT}"),
            IpAddr::V6(_) => format!("[{ip}]:{DEFAULT_SYNC_PORT}"),
        });
    }
    if value.parse::<std::net::SocketAddr>().is_ok() || value.contains(':') {
        Ok(value.to_string())
    } else {
        Ok(format!("{value}:{DEFAULT_SYNC_PORT}"))
    }
}

pub fn new_request_id() -> anyhow::Result<String> {
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).map_err(|error| anyhow!("entropy unavailable: {error}"))?;
    Ok(hex(&id))
}

/// Start a fresh attempt. First contact is intentionally unpinned; the
/// certificate observed on that exact TLS connection is checked against the
/// master's declared node id and returned as a durable retry target.
pub async fn request_join(
    catalog: &Arc<Catalog>,
    identity: &NodeIdentity,
    endpoint: &str,
    timeout: Duration,
) -> anyhow::Result<JoinOutcome> {
    ensure_not_enrolled(catalog).await?;
    let endpoint = normalize_master_endpoint(endpoint)?;
    let request_id = new_request_id()?;
    exchange(catalog, identity, &endpoint, &request_id, None, timeout).await
}

/// Poll a durable pending request. Unlike first contact, every poll pins the
/// certificate captured by that request.
pub async fn poll_join(
    catalog: &Arc<Catalog>,
    identity: &NodeIdentity,
    target: &PendingJoinTarget,
    timeout: Duration,
) -> anyhow::Result<JoinOutcome> {
    let fingerprint = unhex::<32>(&target.master_fingerprint)
        .ok_or_else(|| anyhow!("pending join has a malformed master fingerprint"))?;
    if NodeId::parse_hex(&target.request_id).is_none() {
        return Err(anyhow!("pending join has a malformed request id"));
    }
    exchange(
        catalog,
        identity,
        &target.endpoint,
        &target.request_id,
        Some(fingerprint),
        timeout,
    )
    .await
}

async fn ensure_not_enrolled(catalog: &Arc<Catalog>) -> anyhow::Result<()> {
    let catalog = catalog.clone();
    let peers = tokio::task::spawn_blocking(move || catalog.fleet_peers())
        .await
        .context("checking the local fleet roster")??;
    if let Some(existing) = peers
        .into_iter()
        .find(|peer| peer.role == PeerRole::Central)
    {
        return Err(anyhow!(
            "already joined to master {} ({}); leave it before joining another",
            existing.name,
            existing.node_id
        ));
    }
    Ok(())
}

async fn exchange(
    catalog: &Arc<Catalog>,
    identity: &NodeIdentity,
    endpoint: &str,
    request_id: &str,
    pinned: Option<[u8; 32]>,
    timeout: Duration,
) -> anyhow::Result<JoinOutcome> {
    let (stream, fingerprint) = if let Some(fingerprint) = pinned {
        let (stream, _) = tls::connect(identity, endpoint, fingerprint, timeout)
            .await
            .context("reaching the master")?;
        (stream, fingerprint)
    } else {
        let (stream, _, fingerprint) = tls::connect_first(identity, endpoint, timeout)
            .await
            .context("reaching the master")?;
        (stream, fingerprint)
    };
    let (mut rd, mut wr) = tokio::io::split(stream);
    let max = wire::DEFAULT_MAX_FRAME_BYTES;
    let request = wire::encode(&Message::JoinRequest {
        request_id: request_id.to_string(),
        name: identity.name.clone(),
        platform: std::env::consts::OS.to_string(),
    })?;
    tokio::time::timeout(timeout, wire::write_frame(&mut wr, &request, max))
        .await
        .map_err(|_| anyhow!("sending the join request timed out"))??;
    let (reply, _) = tokio::time::timeout(timeout, wire::read_frame(&mut rd, max))
        .await
        .map_err(|_| anyhow!("the master did not answer in time"))??;
    match reply {
        Message::JoinPending { node_id, name } => {
            verify_master(node_id, fingerprint)?;
            Ok(JoinOutcome::Pending(PendingJoinTarget {
                request_id: request_id.to_string(),
                endpoint: endpoint.to_string(),
                master_name: name,
                master_fingerprint: hex(&fingerprint),
                requested_at: UnixNanos::now(),
                rejected_reason: None,
            }))
        }
        Message::Joined { node_id, name } => {
            verify_master(node_id, fingerprint)?;
            let peer = FleetPeer {
                node_id,
                name: name.clone(),
                role: PeerRole::Central,
                fingerprint,
                endpoint: Some(endpoint.to_string()),
                enabled: true,
                enrolled_at: UnixNanos::now(),
                last_seen_at: Some(UnixNanos::now()),
                last_error: None,
                connected: false,
            };
            let catalog = catalog.clone();
            tokio::task::spawn_blocking(move || catalog.fleet_upsert_peer(&peer))
                .await
                .context("recording the master in the local fleet roster")??;
            Ok(JoinOutcome::Joined(JoinedFleet {
                master: node_id,
                master_name: name,
                endpoint: endpoint.to_string(),
            }))
        }
        Message::JoinRejected { reason } => Ok(JoinOutcome::Rejected(reason)),
        Message::Goodbye { reason } => Err(anyhow!("the master closed the connection: {reason}")),
        other => Err(anyhow!("unexpected reply {}", other.kind())),
    }
}

fn verify_master(node_id: NodeId, fingerprint: [u8; 32]) -> anyhow::Result<()> {
    if node_id != node_id_of(&fingerprint) {
        return Err(anyhow!(
            "the master identity does not match the certificate it presented"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_addresses_get_a_predictable_default_port() {
        assert_eq!(
            normalize_master_endpoint("10.0.0.4").unwrap(),
            "10.0.0.4:7710"
        );
        assert_eq!(
            normalize_master_endpoint("host.local").unwrap(),
            "host.local:7710"
        );
        assert_eq!(
            normalize_master_endpoint("host.local:9000").unwrap(),
            "host.local:9000"
        );
        assert_eq!(normalize_master_endpoint("::1").unwrap(), "[::1]:7710");
        assert_eq!(normalize_master_endpoint("[::1]").unwrap(), "[::1]:7710");
        assert!(normalize_master_endpoint("https://host").is_err());
    }
}
