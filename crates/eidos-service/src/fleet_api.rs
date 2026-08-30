//! Fleet endpoints: status, role and listener configuration, invitations,
//! enrollment, peer maintenance, and per-source sync policy.
//!
//! These are operator actions on the loopback API, the same trust level as
//! adding a source. The fleet's own trust boundary is the dedicated TLS
//! endpoint in `eidos-fleet`, never this API.

use crate::api::{blocking, source_view, ApiError, ApiResult, SourceView};
use crate::api_json::ApiJson;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use eidos_catalog::fleet::{NodeId, PeerRole};
use eidos_domain::{SourceId, SourceKind, SyncPolicy, UnixNanos};
use eidos_fleet::enroll::{create_invite, enroll};
use eidos_fleet::status::PeerView;
use eidos_fleet::{Fleet, FleetConfig, FleetStatus, InviteCode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use ts_rs::TS;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/fleet", get(status))
        .route("/fleet/central", post(set_central))
        .route("/fleet/invite", post(invite))
        .route("/fleet/enroll", post(enroll_node))
        .route("/fleet/sync", post(set_sync))
        .route("/fleet/leave", post(leave))
        .route("/fleet/peers/{id}", post(update_peer).delete(forget_peer))
        .route("/sources/{id}/sync-policy", post(set_sync_policy))
}

fn fleet(st: &AppState) -> Result<Arc<Fleet>, ApiError> {
    st.fleet
        .lock()
        .clone()
        .ok_or_else(|| ApiError::unavailable("the fleet runtime is not running", None))
}

fn parse_node(id: &str) -> Result<NodeId, ApiError> {
    NodeId::parse_hex(id).ok_or_else(|| ApiError::bad_request("node id must be 32 hex characters"))
}

async fn status(State(st): State<Arc<AppState>>) -> ApiResult<FleetStatus> {
    let fleet = fleet(&st)?;
    Ok(ApiJson(read_status(fleet).await?))
}

async fn read_status(fleet: Arc<Fleet>) -> Result<FleetStatus, ApiError> {
    blocking(move || Ok(fleet.status())).await
}

#[derive(Debug, Deserialize, TS)]
pub struct CentralBody {
    /// Accept enrollments and replicate enrolled nodes' sources here.
    #[ts(optional)]
    pub central: Option<bool>,
    /// Sync listener address (`host:port`); `""` stops listening.
    #[ts(optional)]
    pub listen: Option<String>,
}

async fn set_central(
    State(st): State<Arc<AppState>>,
    Json(body): Json<CentralBody>,
) -> ApiResult<FleetConfig> {
    let fleet = fleet(&st)?;
    let central = body.central;
    let listen = body
        .listen
        .map(|listen| listen.trim().to_string())
        .map(|listen| -> Result<Option<String>, ApiError> {
            if listen.is_empty() {
                Ok(None)
            } else {
                listen
                    .parse::<std::net::SocketAddr>()
                    .map_err(|e| ApiError::bad_request(format!("listen address: {e}")))?;
                Ok(Some(listen))
            }
        })
        .transpose()?;
    let config = blocking(move || {
        fleet
            .update_config(move |config| {
                if let Some(central) = central {
                    config.central = central;
                }
                if let Some(listen) = listen {
                    config.listen = listen;
                }
                Ok(())
            })
            .map_err(|e| ApiError::internal(e.to_string()))
    })
    .await?;
    Ok(ApiJson(config))
}

#[derive(Debug, Deserialize, TS)]
pub struct InviteBody {
    /// How nodes reach this central (`host:port`); defaults to this host's
    /// name and the listener's port.
    #[ts(optional)]
    pub endpoint: Option<String>,
    #[ts(optional)]
    pub name_hint: Option<String>,
}

#[derive(Debug, Serialize, TS)]
pub struct InviteView {
    pub code: String,
    pub endpoint: String,
    pub expires_at: UnixNanos,
}

fn default_endpoint(st: &AppState, config: &FleetConfig) -> Result<String, ApiError> {
    let listen = config.listen.as_deref().ok_or_else(|| {
        ApiError::bad_request(
            "no sync listener is configured; set one with POST /api/fleet/central",
        )
    })?;
    let addr: std::net::SocketAddr = listen
        .parse()
        .map_err(|e| ApiError::bad_request(format!("listen address: {e}")))?;
    Ok(advertised_endpoint(&st.host_name, addr))
}

fn advertised_endpoint(host_name: &str, addr: std::net::SocketAddr) -> String {
    if addr.ip().is_unspecified() {
        format!("{host_name}:{}", addr.port())
    } else {
        // SocketAddr formatting preserves the brackets required by IPv6.
        std::net::SocketAddr::new(addr.ip(), addr.port()).to_string()
    }
}

async fn invite(
    State(st): State<Arc<AppState>>,
    Json(body): Json<InviteBody>,
) -> ApiResult<InviteView> {
    let fleet = fleet(&st)?;
    let config = fleet.config();
    let endpoint = match body
        .endpoint
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
    {
        Some(e) => e,
        None => default_endpoint(&st, &config)?,
    };
    let invite_endpoint = endpoint.clone();
    let code = blocking(move || {
        create_invite(
            &st.catalog,
            fleet.identity(),
            &config,
            &invite_endpoint,
            body.name_hint.as_deref(),
        )
        .map_err(|e| ApiError::bad_request(e.to_string()))
    })
    .await?;
    Ok(ApiJson(InviteView {
        code: code.encode(),
        endpoint,
        expires_at: UnixNanos(
            UnixNanos::now().0 + eidos_fleet::enroll::INVITE_TTL.as_nanos() as i64,
        ),
    }))
}

#[derive(Debug, Deserialize, TS)]
pub struct EnrollBody {
    pub code: String,
}

#[derive(Debug, Serialize, TS)]
pub struct EnrollView {
    pub central: NodeId,
    pub central_name: String,
    pub endpoint: String,
}

async fn enroll_node(
    State(st): State<Arc<AppState>>,
    Json(body): Json<EnrollBody>,
) -> ApiResult<EnrollView> {
    let fleet = fleet(&st)?;
    let code = InviteCode::parse(&body.code).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let outcome = enroll(
        &st.catalog,
        fleet.identity(),
        &code,
        Duration::from_secs(15),
    )
    .await
    .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    Ok(ApiJson(EnrollView {
        central: outcome.central,
        central_name: outcome.central_name,
        endpoint: outcome.endpoint,
    }))
}

#[derive(Debug, Deserialize, TS)]
pub struct SyncBody {
    pub enabled: bool,
}

fn central_peer(st: &AppState) -> Result<eidos_catalog::fleet::FleetPeer, ApiError> {
    st.catalog
        .fleet_peers()?
        .into_iter()
        .find(|p| p.role == PeerRole::Central)
        .ok_or_else(|| ApiError::bad_request("this node is not enrolled with a central"))
}

/// Pause or resume transfer without forgetting anything: the ledger and
/// cursors stay, so a later resume needs no resync.
async fn set_sync(
    State(st): State<Arc<AppState>>,
    Json(body): Json<SyncBody>,
) -> ApiResult<FleetStatus> {
    let runtime = fleet(&st)?;
    let enabled = body.enabled;
    let catalog_state = st.clone();
    blocking(move || {
        let central = central_peer(&catalog_state)?;
        catalog_state
            .catalog
            .fleet_set_peer_enabled(central.node_id, enabled)?;
        Ok(())
    })
    .await?;
    if !body.enabled {
        // Sessions re-check the roster only when they start; close the
        // running one so transfer stops now.
        runtime.registry().close_all();
    }
    Ok(ApiJson(read_status(runtime).await?))
}

/// Leave the fleet: forget the central. The maintenance loop then removes
/// the ledgers; local indexes are untouched. Rejoining is a new epoch.
async fn leave(State(st): State<Arc<AppState>>) -> ApiResult<FleetStatus> {
    let runtime = fleet(&st)?;
    let catalog_state = st.clone();
    blocking(move || {
        let central = central_peer(&catalog_state)?;
        catalog_state.catalog.fleet_remove_peer(central.node_id)?;
        Ok(())
    })
    .await?;
    runtime.registry().close_all();
    Ok(ApiJson(read_status(runtime).await?))
}

#[derive(Debug, Deserialize, TS)]
pub struct PeerBody {
    /// Where this side dials the peer (`host:port`); `""` clears it.
    #[ts(optional)]
    pub endpoint: Option<String>,
    #[ts(optional)]
    pub enabled: Option<bool>,
}

async fn update_peer(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PeerBody>,
) -> ApiResult<PeerView> {
    let node = parse_node(&id)?;
    let fleet = fleet(&st)?;
    let disabled = body.enabled == Some(false);
    let catalog_state = st.clone();
    blocking(move || {
        if let Some(endpoint) = body.endpoint {
            let endpoint = endpoint.trim().to_string();
            catalog_state.catalog.fleet_set_peer_endpoint(
                node,
                (!endpoint.is_empty()).then_some(endpoint.as_str()),
            )?;
        }
        if let Some(enabled) = body.enabled {
            catalog_state
                .catalog
                .fleet_set_peer_enabled(node, enabled)?;
        }
        Ok(())
    })
    .await?;
    if disabled {
        // Persist revocation first, then wake the exact authenticated
        // session so it cannot process another periodic tick of frames.
        fleet.registry().close_peer(node);
    }
    let status = read_status(fleet).await?;
    status
        .peers
        .into_iter()
        .find(|p| p.node_id == node)
        .map(ApiJson)
        .ok_or_else(|| ApiError::not_found(format!("peer {node}")))
}

#[derive(Debug, Serialize, TS)]
pub struct ForgetView {
    pub retired_sources: u64,
}

/// Forget a node: its replicated sources are retired (hidden from search)
/// and its credential no longer admits it.
async fn forget_peer(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<ForgetView> {
    let node = parse_node(&id)?;
    let catalog_state = st.clone();
    let replica_sources = blocking(move || {
        catalog_state
            .catalog
            .fleet_forget_peer(node)?
            .ok_or_else(|| ApiError::not_found(format!("peer {node}")))
    })
    .await?;
    if let Some(fleet) = st.fleet.lock().clone() {
        fleet.registry().close_peer(node);
    }
    let retired = blocking(move || {
        let mut retired = 0;
        for source in replica_sources {
            if st.catalog.replica_retire_source(source)? {
                retired += 1;
            }
        }
        Ok(retired)
    })
    .await?;
    Ok(ApiJson(ForgetView {
        retired_sources: retired,
    }))
}

#[derive(Debug, Deserialize, TS)]
pub struct SyncPolicyBody {
    pub policy: SyncPolicy,
}

async fn set_sync_policy(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<SyncPolicyBody>,
) -> ApiResult<SourceView> {
    let sid = SourceId(id);
    let view = blocking(move || {
        let s = st
            .catalog
            .get_source(sid)?
            .ok_or_else(|| ApiError::not_found(format!("source {id}")))?;
        if s.kind == SourceKind::Remote {
            return Err(ApiError::bad_request(
                "a replicated source has no sync policy of its own",
            ));
        }
        st.catalog.set_sync_policy(sid, body.policy)?;
        let s = st
            .catalog
            .get_source(sid)?
            .ok_or_else(|| ApiError::not_found(format!("source {id}")))?;
        source_view(&st, s)
    })
    .await?;
    Ok(ApiJson(view))
}

#[cfg(test)]
mod tests {
    use super::advertised_endpoint;

    #[test]
    fn advertised_ipv6_endpoints_keep_their_brackets() {
        let addr = "[::1]:7710".parse().unwrap();
        assert_eq!(advertised_endpoint("ignored", addr), "[::1]:7710");
        let unspecified = "[::]:7710".parse().unwrap();
        assert_eq!(
            advertised_endpoint("central-host", unspecified),
            "central-host:7710"
        );
    }
}
