//! The fleet runtime: listener, dialers, the session registry, and the
//! maintenance loops. Started by the service inside its tokio runtime and
//! stopped through the shutdown channel.
//!
//! Every loop re-reads `fleet/config.json` and the roster on its own tick,
//! so `eidos fleet ...` commands and enrollments take effect without a
//! restart. Nothing here is on the path of local search, scanning, or
//! shutdown: sessions and maintenance run on their own tasks and bounded
//! blocking calls, and a central that is unreachable only ever costs the
//! node a reconnect timer.

use crate::config::FleetConfig;
use crate::identity::{hex, NodeIdentity};
use crate::metrics::FleetCounters;
use crate::session::{run_session, Registry, SessionContext, SessionEnd};
use crate::status::{Direction, FleetStatus, LocalSourceSync, PeerView, ReplicaSourceSync};
use crate::tls;
use anyhow::Context;
use eidos_catalog::fleet::{NodeId, PeerRole};
use eidos_catalog::Catalog;
use eidos_domain::{SourceKind, SourceState, SyncPolicy, UnixNanos};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinHandle;

const CONFIG_POLL: Duration = Duration::from_secs(2);
const DIAL_POLL: Duration = Duration::from_secs(2);
const MAINTENANCE_POLL: Duration = Duration::from_secs(5);
const COLLECT_EVERY: Duration = Duration::from_secs(60);
const AGGREGATES_EVERY: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const BACKFILL_STEP: u32 = 2_000;
const COLLECT_LIMIT: u32 = 10_000;
const RECONNECT_MIN: Duration = Duration::from_secs(2);
/// Bound unauthenticated TLS work and established inbound sessions. Once
/// full, the OS listener backlog provides the next layer of backpressure.
const MAX_INBOUND_CONNECTIONS: usize = 64;

struct Dial {
    next: Instant,
    delay: Duration,
    in_progress: bool,
}

/// Handle the service holds: status, control, and shutdown.
pub struct Fleet {
    ctx: Arc<SessionContext>,
    data_dir: PathBuf,
    shutdown: watch::Sender<bool>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    listening: Arc<Mutex<Option<String>>>,
    listener_error: Arc<Mutex<Option<String>>>,
    dials: Arc<Mutex<HashMap<NodeId, Dial>>>,
    /// Sources whose ledger looks over its ceiling, by name.
    degraded: Arc<Mutex<Vec<String>>>,
}

impl Fleet {
    /// Load (or create) the identity and start every loop. Must be called
    /// on a tokio runtime.
    pub fn start(catalog: Arc<Catalog>, data_dir: &std::path::Path) -> anyhow::Result<Arc<Fleet>> {
        let config = FleetConfig::load(data_dir).unwrap_or_else(|e| {
            tracing::error!(error = %e, "fleet config unreadable; running with defaults until it is fixed");
            FleetConfig::default()
        });
        let identity = NodeIdentity::load_or_create(data_dir, &eidos_domain::bench::hostname())
            .context("fleet identity")?;
        tracing::info!(node = %identity.node_id, name = %identity.name, fingerprint = %identity.fingerprint_hex(), central = config.central, "fleet identity loaded");
        let ctx = Arc::new(SessionContext {
            identity: Arc::new(identity),
            catalog,
            config: Arc::new(RwLock::new(config)),
            counters: Arc::new(FleetCounters::default()),
            registry: Arc::new(Registry::default()),
            platform: std::env::consts::OS.to_string(),
        });
        let (shutdown, _) = watch::channel(false);
        let fleet = Arc::new(Fleet {
            ctx,
            data_dir: data_dir.to_path_buf(),
            shutdown,
            tasks: Mutex::new(Vec::new()),
            listening: Arc::new(Mutex::new(None)),
            listener_error: Arc::new(Mutex::new(None)),
            dials: Arc::new(Mutex::new(HashMap::new())),
            degraded: Arc::new(Mutex::new(Vec::new())),
        });
        let mut tasks = fleet.tasks.lock();
        tasks.push(tokio::spawn(config_loop(fleet.clone())));
        tasks.push(tokio::spawn(listener_loop(fleet.clone())));
        tasks.push(tokio::spawn(dial_loop(fleet.clone())));
        tasks.push(tokio::spawn(maintenance_loop(fleet.clone())));
        drop(tasks);
        Ok(fleet)
    }

    pub fn identity(&self) -> &NodeIdentity {
        &self.ctx.identity
    }

    pub fn config(&self) -> FleetConfig {
        self.ctx.config.read().clone()
    }

    pub fn counters(&self) -> &FleetCounters {
        &self.ctx.counters
    }

    pub fn registry(&self) -> &Registry {
        &self.ctx.registry
    }

    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// Whether a session with `peer` is open right now.
    pub fn is_connected(&self, peer: NodeId) -> bool {
        self.ctx.registry.is_connected(peer)
    }

    /// Stop every session and loop. Sessions say goodbye; durable state
    /// needs nothing from them.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
        self.ctx.registry.close_all();
        // Let listener_loop observe shutdown and abort its owned accept task.
        // Aborting the parent here would drop (and detach) that JoinHandle,
        // leaving the sync socket live after Fleet::shutdown returned.
    }

    /// Reload the configuration file now (after a CLI command changed it).
    pub fn reload_config(&self) -> anyhow::Result<()> {
        let config = FleetConfig::load(&self.data_dir)?;
        *self.ctx.config.write() = config;
        Ok(())
    }

    pub fn status(&self) -> FleetStatus {
        let catalog = &self.ctx.catalog;
        let config = self.config();
        let connected = self.ctx.registry.connected_peers();
        let dials = self.dials.lock();
        let peers: Vec<PeerView> = catalog
            .fleet_peers()
            .unwrap_or_default()
            .into_iter()
            .map(|p| PeerView {
                node_id: p.node_id,
                name: p.name,
                role: p.role.as_str().into(),
                fingerprint: hex(&p.fingerprint),
                endpoint: p.endpoint,
                enabled: p.enabled,
                connected: connected.contains(&p.node_id),
                last_seen_at: p.last_seen_at,
                last_error: p.last_error,
                next_dial_in_ms: dials
                    .get(&p.node_id)
                    .filter(|_| !connected.contains(&p.node_id))
                    .map(|d| d.next.saturating_duration_since(Instant::now()).as_millis() as u64),
            })
            .collect();
        drop(dials);
        let central = peers.iter().find(|p| p.role == "central");
        let mut degraded = self.degraded.lock().clone();
        if let Some(e) = self.listener_error.lock().clone() {
            degraded.push(format!("sync listener: {e}"));
        }
        let mut local_sources = Vec::new();
        let mut replica_sources = Vec::new();
        let central_id = central.map(|c| c.node_id);
        for s in catalog.list_sources().unwrap_or_default() {
            if s.kind == SourceKind::Remote {
                if let Ok(Some(cov)) = catalog.replica_coverage(s.id) {
                    replica_sources.push(ReplicaSourceSync {
                        source_id: s.id,
                        name: s.name.clone(),
                        node: NodeId(cov.node_id),
                        node_name: cov.node_name,
                        remote_source_id: cov.remote_source_id,
                        epoch: cov.epoch.to_string(),
                        applied_seq: cov.applied_seq,
                        reported_head: cov.reported_head,
                        applied_at: cov.applied_at,
                        reported_at: cov.reported_at,
                        resyncing: cov.resyncing,
                        connected: connected.contains(&NodeId(cov.node_id)),
                    });
                }
                continue;
            }
            let ledger = catalog.sync_source(s.id).ok().flatten();
            let backlog = match (&ledger, central_id) {
                (Some(_), Some(c)) => catalog.sync_backlog(s.id, c.0).unwrap_or_default(),
                _ => Default::default(),
            };
            let over = backlog.rows > config.backlog_ceiling_rows
                || backlog.tombstones > config.backlog_ceiling_tombstones;
            local_sources.push(LocalSourceSync {
                source_id: s.id,
                name: s.name.clone(),
                policy: s.sync_policy.as_str().into(),
                enabled: ledger.is_some(),
                ready: ledger.as_ref().is_some_and(|l| l.ready),
                epoch: ledger.as_ref().map(|l| l.epoch.to_string()),
                head_seq: ledger.as_ref().map(|l| l.head_seq).unwrap_or(0),
                compacted_through: ledger.as_ref().map(|l| l.compacted_through).unwrap_or(0),
                backlog_rows: backlog.rows,
                backlog_tombstones: backlog.tombstones,
                backlog_oldest_age_ms: backlog
                    .oldest_touched_at
                    .map(|t| (UnixNanos::now().as_millis() - t.as_millis()).max(0) as u64),
                degraded: over,
            });
        }
        for s in local_sources.iter().filter(|s| s.degraded) {
            degraded.push(format!(
                "source {} backlog over its ceiling ({} rows, {} tombstones)",
                s.name, s.backlog_rows, s.backlog_tombstones
            ));
        }
        FleetStatus {
            node_id: self.ctx.identity.node_id,
            name: self.ctx.identity.name.clone(),
            fingerprint: self.ctx.identity.fingerprint_hex(),
            central: config.central,
            enrolled: central.is_some(),
            sync_enabled: central.is_some_and(|c| c.enabled),
            listen: config.listen.clone(),
            listening: self.listening.lock().clone(),
            peers,
            sessions: self.ctx.registry.views(),
            local_sources,
            replica_sources,
            counters: self.ctx.counters.view(),
            degraded,
            pending_invites: catalog.fleet_pending_invites().unwrap_or(0),
        }
    }
}

async fn config_loop(fleet: Arc<Fleet>) {
    let mut shutdown = fleet.shutdown.subscribe();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(CONFIG_POLL) => {
                match FleetConfig::load(&fleet.data_dir) {
                    Ok(config) => {
                        let mut current = fleet.ctx.config.write();
                        if *current != config {
                            tracing::info!(central = config.central, listen = ?config.listen, "fleet configuration changed");
                            *current = config;
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "fleet configuration unreadable; keeping the previous one"),
                }
            }
            _ = shutdown.changed() => return,
        }
    }
}

/// Bind the sync listener named by the configuration and accept sessions;
/// rebind when the address changes.
async fn listener_loop(fleet: Arc<Fleet>) {
    let mut shutdown = fleet.shutdown.subscribe();
    let mut current: Option<(String, JoinHandle<()>)> = None;
    loop {
        let wanted = fleet.ctx.config.read().listen.clone();
        let changed = current.as_ref().map(|(addr, _)| addr) != wanted.as_ref();
        if changed {
            if let Some((_, task)) = current.take() {
                task.abort();
                *fleet.listening.lock() = None;
            }
            if let Some(addr) = wanted.clone() {
                match tokio::net::TcpListener::bind(&addr).await {
                    Ok(listener) => {
                        let local = listener
                            .local_addr()
                            .map(|a| a.to_string())
                            .unwrap_or(addr.clone());
                        tracing::info!(listen = %local, "fleet sync listener bound");
                        *fleet.listening.lock() = Some(local);
                        *fleet.listener_error.lock() = None;
                        let f = fleet.clone();
                        current = Some((addr, tokio::spawn(accept_loop(f, listener))));
                    }
                    Err(e) => {
                        tracing::error!(listen = %addr, error = %e, "fleet sync listener failed to bind");
                        *fleet.listener_error.lock() = Some(format!("{addr}: {e}"));
                    }
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(CONFIG_POLL) => {}
            _ = shutdown.changed() => {
                if let Some((_, task)) = current.take() {
                    task.abort();
                }
                return;
            }
        }
    }
}

async fn accept_loop(fleet: Arc<Fleet>, listener: tokio::net::TcpListener) {
    let config = match tls::server_config(&fleet.ctx.identity) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "fleet TLS server configuration failed");
            *fleet.listener_error.lock() = Some(e.to_string());
            return;
        }
    };
    let permits = Arc::new(Semaphore::new(MAX_INBOUND_CONNECTIONS));
    loop {
        let permit = match permits.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => return,
        };
        let (tcp, addr) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                drop(permit);
                tracing::warn!(error = %e, "fleet accept failed");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let fleet = fleet.clone();
        let config = config.clone();
        tokio::spawn(async move {
            let _permit = permit;
            fleet
                .ctx
                .counters
                .add(&fleet.ctx.counters.connections_attempted, 1);
            match tls::accept(config, tcp, HANDSHAKE_TIMEOUT).await {
                Ok((stream, fingerprint)) => {
                    let shutdown = fleet.shutdown.subscribe();
                    let end = run_session(
                        fleet.ctx.clone(),
                        stream,
                        fingerprint,
                        Direction::Inbound,
                        Some(addr.to_string()),
                        shutdown,
                    )
                    .await;
                    tracing::debug!(%addr, ?end, "inbound fleet session ended");
                }
                Err(e) => tracing::debug!(%addr, error = %e, "inbound fleet handshake failed"),
            }
        });
    }
}

/// Dial every peer this side is responsible for reaching: the central from
/// a node, and nodes with an endpoint from a central. One session per peer,
/// bounded exponential backoff between attempts.
async fn dial_loop(fleet: Arc<Fleet>) {
    let mut shutdown = fleet.shutdown.subscribe();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(DIAL_POLL) => {}
            _ = shutdown.changed() => return,
        }
        let (central_role, max_delay) = {
            let cfg = fleet.ctx.config.read();
            (
                cfg.central,
                Duration::from_secs(cfg.reconnect_max_secs.max(2)),
            )
        };
        let peers = match fleet.ctx.catalog.fleet_peers() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "fleet roster unreadable");
                continue;
            }
        };
        let now = Instant::now();
        for peer in peers {
            let Some(endpoint) = peer.endpoint.clone().filter(|e| !e.is_empty()) else {
                continue;
            };
            if !peer.enabled {
                continue;
            }
            let my_job = match peer.role {
                PeerRole::Central => !central_role,
                PeerRole::Node => central_role,
            };
            if !my_job || fleet.ctx.registry.is_connected(peer.node_id) {
                continue;
            }
            let mut dials = fleet.dials.lock();
            let dial = dials.entry(peer.node_id).or_insert(Dial {
                next: now,
                delay: RECONNECT_MIN,
                in_progress: false,
            });
            if dial.in_progress || dial.next > now {
                continue;
            }
            dial.in_progress = true;
            drop(dials);
            let fleet = fleet.clone();
            tokio::spawn(async move {
                fleet
                    .ctx
                    .counters
                    .add(&fleet.ctx.counters.connections_attempted, 1);
                let result = tls::connect(
                    &fleet.ctx.identity,
                    &endpoint,
                    peer.fingerprint,
                    CONNECT_TIMEOUT,
                )
                .await;
                let outcome = match result {
                    Ok((stream, addr)) => {
                        let shutdown = fleet.shutdown.subscribe();
                        let end = run_session(
                            fleet.ctx.clone(),
                            stream,
                            peer.fingerprint,
                            Direction::Outbound,
                            Some(addr.to_string()),
                            shutdown,
                        )
                        .await;
                        tracing::debug!(peer = %peer.node_id, ?end, "outbound fleet session ended");
                        end
                    }
                    Err(e) => {
                        tracing::debug!(peer = %peer.node_id, %endpoint, error = %e, "fleet dial failed");
                        let _ = fleet
                            .ctx
                            .catalog
                            .fleet_note_peer_seen(peer.node_id, Some(&e.to_string()));
                        SessionEnd::Failed(e.to_string())
                    }
                };
                let mut dials = fleet.dials.lock();
                let dial = dials.entry(peer.node_id).or_insert(Dial {
                    next: Instant::now(),
                    delay: RECONNECT_MIN,
                    in_progress: false,
                });
                dial.in_progress = false;
                match outcome {
                    // A session that ran for a while restarts the backoff.
                    SessionEnd::Closed | SessionEnd::Enrolled(_) => {
                        dial.delay = RECONNECT_MIN;
                    }
                    // The other direction won; wait a little before trying
                    // again so the surviving session has time to settle.
                    SessionEnd::Duplicate => {
                        dial.delay = RECONNECT_MIN;
                    }
                    SessionEnd::Failed(_) | SessionEnd::UnknownPeer | SessionEnd::Version => {
                        dial.delay = (dial.delay * 2).min(max_delay);
                    }
                }
                let jitter = Duration::from_millis(getrandom_u64() % 1_000);
                dial.next = Instant::now() + dial.delay + jitter;
            });
        }
    }
}

fn getrandom_u64() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).ok();
    u64::from_le_bytes(b)
}

/// Journal identity of a source's native feed checkpoint, when it has one.
fn journal_id(catalog: &Catalog, source: eidos_domain::SourceId) -> Option<i64> {
    let (checkpoint, _) = catalog.checkpoint(source).ok().flatten()?;
    if checkpoint.kind != "usn" {
        return None;
    }
    checkpoint
        .value
        .get("journal_id")
        .and_then(|v| v.as_u64())
        .map(|v| v as i64)
}

/// Node side: keep every eligible source's ledger enabled and backfilled
/// while a central is configured, mint a new epoch when a native journal
/// is replaced, collect acknowledged tombstones, and measure the backlog.
/// Central side: refresh aggregates of replicated sources that changed.
async fn maintenance_loop(fleet: Arc<Fleet>) {
    let mut shutdown = fleet.shutdown.subscribe();
    let mut last_collect = Instant::now();
    let mut last_aggregates: HashMap<eidos_domain::SourceId, (Instant, Option<UnixNanos>)> =
        HashMap::new();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(MAINTENANCE_POLL) => {}
            _ = shutdown.changed() => return,
        }
        let fleet2 = fleet.clone();
        let collect_now = last_collect.elapsed() > COLLECT_EVERY;
        if collect_now {
            last_collect = Instant::now();
        }
        let mut aggregates = std::mem::take(&mut last_aggregates);
        let result = tokio::task::spawn_blocking(move || {
            maintain(&fleet2, collect_now, &mut aggregates);
            aggregates
        })
        .await;
        if let Ok(a) = result {
            last_aggregates = a;
        }
    }
}

fn maintain(
    fleet: &Fleet,
    collect_now: bool,
    aggregates: &mut HashMap<eidos_domain::SourceId, (Instant, Option<UnixNanos>)>,
) {
    let catalog = &fleet.ctx.catalog;
    let config = fleet.config();
    let counters = &fleet.ctx.counters;
    let peers = catalog.fleet_peers().unwrap_or_default();
    let central = peers.iter().find(|p| p.role == PeerRole::Central);
    let sources = match catalog.list_sources() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "fleet maintenance could not list sources");
            return;
        }
    };
    let mut degraded = Vec::new();
    for s in &sources {
        if s.kind == SourceKind::Remote {
            continue;
        }
        let eligible = s.state != SourceState::Retired
            && s.published_generation.is_some()
            && s.sync_policy == SyncPolicy::Inherit;
        let ledger = catalog.sync_source(s.id).ok().flatten();
        match (central, eligible, ledger) {
            // Enrolled (even if sync is paused) and eligible: keep the
            // ledger, mint it when missing, and re-mint on a journal change.
            (Some(_), true, None) => {
                let journal = journal_id(catalog, s.id);
                match catalog.sync_enable(s.id, journal) {
                    Ok(state) => {
                        tracing::info!(source = s.id.0, epoch = %state.epoch, "sync enabled for source")
                    }
                    Err(e) => tracing::warn!(source = s.id.0, error = %e, "could not enable sync"),
                }
            }
            (Some(_), true, Some(ledger)) => {
                let journal = journal_id(catalog, s.id);
                if journal.is_some() && ledger.journal_id.is_some() && journal != ledger.journal_id
                {
                    tracing::warn!(
                        source = s.id.0,
                        "native journal replaced; minting a new sync epoch"
                    );
                    if let Err(e) = catalog.sync_enable(s.id, journal) {
                        tracing::warn!(source = s.id.0, error = %e, "could not re-enable sync");
                    }
                    continue;
                }
                if !ledger.ready {
                    let mut steps = 0;
                    loop {
                        match catalog.sync_backfill(s.id, BACKFILL_STEP) {
                            Ok(p) => {
                                counters.add(&counters.backfill_steps, 1);
                                steps += 1;
                                if p.done || steps >= 8 {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(source = s.id.0, error = %e, "sync backfill step failed");
                                break;
                            }
                        }
                    }
                }
                if collect_now {
                    match catalog.sync_collect(s.id, COLLECT_LIMIT) {
                        Ok(stats) => {
                            counters.add(&counters.collections, 1);
                            counters.add(&counters.tombstones_collected, stats.removed_tombstones);
                        }
                        Err(e) => {
                            tracing::warn!(source = s.id.0, error = %e, "sync collection failed")
                        }
                    }
                }
                if let Some(c) = central {
                    if let Ok(b) = catalog.sync_backlog(s.id, c.node_id.0) {
                        if b.rows > config.backlog_ceiling_rows
                            || b.tombstones > config.backlog_ceiling_tombstones
                        {
                            degraded.push(format!(
                                "source {} backlog over its ceiling ({} rows, {} tombstones)",
                                s.name, b.rows, b.tombstones
                            ));
                        }
                    }
                }
            }
            // No central at all: a standalone does no fleet work.
            (None, _, Some(_)) => {
                if let Err(e) = catalog.sync_disable(s.id) {
                    tracing::warn!(source = s.id.0, error = %e, "could not disable sync");
                } else {
                    tracing::info!(source = s.id.0, "sync disabled: no central is configured");
                }
            }
            // Excluded or ineligible while enrolled: stop shipping it.
            (Some(_), false, Some(_)) => {
                if let Err(e) = catalog.sync_disable(s.id) {
                    tracing::warn!(source = s.id.0, error = %e, "could not disable sync");
                } else {
                    tracing::info!(
                        source = s.id.0,
                        policy = s.sync_policy.as_str(),
                        "sync disabled for source"
                    );
                }
            }
            (None, _, None) | (Some(_), false, None) => {}
        }
    }
    *fleet.degraded.lock() = degraded;

    if config.central {
        for r in catalog.replica_sources().unwrap_or_default() {
            let entry = aggregates
                .entry(r.source_id)
                .or_insert((Instant::now() - AGGREGATES_EVERY * 2, None));
            let changed = entry.1 != r.applied_at;
            if changed && entry.0.elapsed() > AGGREGATES_EVERY {
                if let Err(e) = catalog.replica_rebuild_aggregates(r.source_id) {
                    tracing::warn!(source = r.source_id.0, error = %e, "replica aggregates rebuild failed");
                }
                *entry = (Instant::now(), r.applied_at);
            }
        }
        let _ = catalog.fleet_prune_invites();
    }
}
