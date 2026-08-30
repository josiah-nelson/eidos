//! Enrollment: a central mints a single-use invitation carrying its own
//! fingerprint; a node redeems it over a connection pinned to that
//! fingerprint, and both sides end up with the other in their roster.

use crate::identity::{hex, InviteCode, NodeIdentity};
use crate::tls;
use crate::wire::{self, Message};
use crate::FleetConfig;
use anyhow::{anyhow, Context};
use eidos_catalog::fleet::{FleetPeer, NodeId, PeerRole};
use eidos_catalog::Catalog;
use eidos_domain::UnixNanos;
use std::time::Duration;

/// How long an invitation stays redeemable.
pub const INVITE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Mint an invitation on a central. `endpoint` is how nodes will reach this
/// central's sync listener (host:port); it travels in the code.
pub fn create_invite(
    catalog: &Catalog,
    identity: &NodeIdentity,
    config: &FleetConfig,
    endpoint: &str,
    name_hint: Option<&str>,
) -> anyhow::Result<InviteCode> {
    if !config.central {
        return Err(anyhow!(
            "this installation is not a central; run `eidos fleet central --listen <addr>` first"
        ));
    }
    if endpoint.trim().is_empty() {
        return Err(anyhow!(
            "an invitation needs the central's endpoint (host:port)"
        ));
    }
    let code = InviteCode::generate(identity.fingerprint, endpoint.trim())?;
    let expires = UnixNanos(UnixNanos::now().0 + INVITE_TTL.as_nanos() as i64);
    catalog.fleet_add_invite(InviteCode::token_hash(&code.secret), name_hint, expires)?;
    let _ = catalog.fleet_prune_invites();
    Ok(code)
}

/// What enrollment established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrollment {
    pub central: NodeId,
    pub central_name: String,
    pub endpoint: String,
}

/// Redeem an invitation from a node: connect pinned to the central's
/// fingerprint, present the secret, and record the central as this node's
/// peer. Sync starts at the service's next tick.
pub async fn enroll(
    catalog: &Catalog,
    identity: &NodeIdentity,
    code: &InviteCode,
    timeout: Duration,
) -> anyhow::Result<Enrollment> {
    if let Some(existing) = catalog
        .fleet_peers()?
        .into_iter()
        .find(|p| p.role == PeerRole::Central)
    {
        if existing.fingerprint != code.central_fingerprint {
            return Err(anyhow!(
                "already enrolled with central {} ({}); leave it first with `eidos fleet leave`",
                existing.name,
                existing.node_id
            ));
        }
    }
    let (stream, _) = tls::connect(identity, &code.endpoint, code.central_fingerprint, timeout)
        .await
        .context("reaching the central")?;
    let (mut rd, mut wr) = tokio::io::split(stream);
    let max = wire::DEFAULT_MAX_FRAME_BYTES;
    let request = wire::encode(&Message::Enroll {
        secret: hex(&code.secret),
        name: identity.name.clone(),
        platform: std::env::consts::OS.to_string(),
    })?;
    wire::write_frame(&mut wr, &request, max).await?;
    let (reply, _) = tokio::time::timeout(timeout, wire::read_frame(&mut rd, max))
        .await
        .map_err(|_| anyhow!("the central did not answer in time"))??;
    match reply {
        Message::Enrolled { node_id, name } => {
            let peer = FleetPeer {
                node_id,
                name: name.clone(),
                role: PeerRole::Central,
                fingerprint: code.central_fingerprint,
                endpoint: Some(code.endpoint.clone()),
                enabled: true,
                enrolled_at: UnixNanos::now(),
                last_seen_at: Some(UnixNanos::now()),
                last_error: None,
            };
            catalog.fleet_upsert_peer(&peer)?;
            Ok(Enrollment {
                central: node_id,
                central_name: name,
                endpoint: code.endpoint.clone(),
            })
        }
        Message::EnrollRejected { reason } => Err(anyhow!("the central refused: {reason}")),
        Message::Goodbye { reason } => Err(anyhow!("the central closed the connection: {reason}")),
        other => Err(anyhow!("unexpected reply {}", other.kind())),
    }
}
