//! One authenticated connection running the duplex protocol.
//!
//! After the TLS handshake each side sends a [`Hello`]; the peer is admitted
//! from the roster (or diverted to enrollment) before any other frame is
//! processed. Both sides then run the same loop: the *shipper* role offers
//! this side's sync-enabled sources and streams materialized batches, the
//! *consumer* role applies the peer's. Which sources exist decides what
//! happens; the initiator gains no authority.
//!
//! Durable progress belongs to the peer identity, never to the connection:
//! the shipper's watermark lives in the catalog's `sync_consumers`, the
//! consumer's cursor in `sync_replica_sources`. A session holds only what
//! is in flight, so a socket that breaks loses nothing and a session that
//! starts in the other direction resumes from the same cursor.

use crate::identity::NodeIdentity;
use crate::metrics::FleetCounters;
use crate::status::{Direction, SessionSourceView, SessionView, SyncRole};
use crate::wire::{self, EnrollmentSecret, Family, Hello, Message, Role, PROTOCOL_VERSION};
use crate::FleetConfig;
use anyhow::{anyhow, Context};
use eidos_catalog::fleet::{FleetPeer, NodeId, PeerRole};
use eidos_catalog::replica::{
    BatchOutcome, HelloOutcome, RemoteNode, RemoteSourceDescriptor, RepairOfferOutcome,
    RepairOutcome,
};
use eidos_catalog::sync::{record_digest, SyncRow, SYNC_ROW_IMAGE_VERSION};
use eidos_catalog::Catalog;
use eidos_domain::{SourceId, SourceKind, SourceState, SyncPolicy, UnixNanos};
use eidos_sync::identity::{ChainHash, SourceEpoch};
use eidos_sync::merkle::{leaf_index, MerkleTree, MAX_FLEET_LEAF_BITS};
use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch, Notify};
use zeroize::{Zeroize, Zeroizing};

/// Interval of the shipper/keepalive tick.
pub const TICK: Duration = Duration::from_secs(1);
/// Silence after which a ping is sent, and after twice which the session
/// is considered dead.
pub const IDLE_PING: Duration = Duration::from_secs(30);
/// Deadline for the hello exchange.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
/// Hello and enrollment are deliberately tiny; unknown certificates never
/// get the full data-frame allocation budget before roster admission.
const ADMISSION_MAX_FRAME_BYTES: usize = 64 * 1024;
/// A peer that stops reading cannot hold a session (or shutdown) forever.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
/// A peer may stay otherwise active while ignoring a source response; bound
/// those protocol phases independently from the connection keepalive.
const PROGRESS_TIMEOUT: Duration = Duration::from_secs(60);
/// Backoff before re-offering a fenced source.
const FENCE_RETRY: Duration = Duration::from_secs(60);
/// A repair offer for a source with fewer objects than this uses a compact
/// tree; larger sources use the fleet minimum.
const SMALL_SOURCE_ROWS: u64 = 1 << 16;
const MIN_LEAF_BITS: u8 = 10;

/// The connection is identified for tie-breaking by who dialed and the
/// random nonce that side chose; both peers know both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionKey {
    pub initiator: NodeId,
    pub nonce: u64,
}

/// Everything a session needs from the runtime.
pub struct SessionContext {
    pub identity: Arc<NodeIdentity>,
    pub catalog: Arc<Catalog>,
    pub config: Arc<RwLock<FleetConfig>>,
    pub counters: Arc<FleetCounters>,
    pub registry: Arc<Registry>,
    pub platform: String,
}

impl SessionContext {
    fn role(&self) -> Role {
        if self.config.read().central {
            Role::Central
        } else {
            Role::Node
        }
    }
}

struct Active {
    key: SessionKey,
    close: Arc<AtomicBool>,
    close_notify: Arc<Notify>,
    view: Arc<Mutex<SessionView>>,
}

/// At most one session per peer. When a second connection to the same peer
/// appears, both sides keep the one with the smaller key and close the
/// other, so simultaneous dials converge on one session without a
/// negotiation round.
#[derive(Default)]
pub struct Registry {
    active: Mutex<HashMap<NodeId, Active>>,
}

#[derive(Debug)]
pub struct Duplicate;

impl Registry {
    fn register(
        &self,
        peer: NodeId,
        key: SessionKey,
        close: Arc<AtomicBool>,
        close_notify: Arc<Notify>,
        view: Arc<Mutex<SessionView>>,
    ) -> Result<(), Duplicate> {
        let mut active = self.active.lock();
        if let Some(existing) = active.get(&peer) {
            if existing.key <= key {
                return Err(Duplicate);
            }
            existing.close.store(true, Ordering::Relaxed);
            existing.close_notify.notify_one();
        }
        active.insert(
            peer,
            Active {
                key,
                close,
                close_notify,
                view,
            },
        );
        Ok(())
    }

    fn unregister(&self, peer: NodeId, key: SessionKey) {
        let mut active = self.active.lock();
        if active.get(&peer).is_some_and(|a| a.key == key) {
            active.remove(&peer);
        }
    }

    pub fn is_connected(&self, peer: NodeId) -> bool {
        self.active.lock().contains_key(&peer)
    }

    pub fn connected_peers(&self) -> Vec<NodeId> {
        self.active.lock().keys().copied().collect()
    }

    pub fn views(&self) -> Vec<SessionView> {
        let mut views: Vec<SessionView> = self
            .active
            .lock()
            .values()
            .map(|a| a.view.lock().clone())
            .collect();
        views.sort_by_key(|v| v.peer);
        views
    }

    /// Ask every session to close (shutdown).
    pub fn close_all(&self) {
        for a in self.active.lock().values() {
            a.close.store(true, Ordering::Relaxed);
            a.close_notify.notify_one();
        }
    }
}

/// How a session ended, for the dialer's backoff and the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEnd {
    /// The peer is not in the roster (and this side is not a central that
    /// could enroll it).
    UnknownPeer,
    /// No common protocol version.
    Version,
    /// Another session to the same peer won the tie-break.
    Duplicate,
    /// The peer enrolled; the connection served only that.
    Enrolled(NodeId),
    /// Orderly close or shutdown.
    Closed,
    /// The connection failed.
    Failed(String),
}

#[derive(Debug, Clone)]
enum ShipPhase {
    Unoffered,
    Offered {
        since: Instant,
    },
    Idle,
    InFlight {
        through_seq: u64,
        bytes: u64,
        since: Instant,
    },
    Repairing {
        through_seq: u64,
        through_chain: ChainHash,
        leaf_bits: u8,
        since: Instant,
    },
    Fenced {
        reason: String,
        since: Instant,
    },
}

struct ShipState {
    epoch: SourceEpoch,
    /// What the peer has, per its own last word (Resume/Ack).
    cursor: u64,
    head: u64,
    phase: ShipPhase,
    batches: u64,
    rows: u64,
    last_error: Option<String>,
}

/// Parts of a repair answer: the leaves each part completes and their rows.
type RepairParts = Vec<(Vec<u32>, Vec<SyncRow>)>;
/// An encoded batch frame with `(through_seq, rows, head_seq)`.
type EncodedBatch = (Vec<u8>, u64, u64, u64);

struct ConsumeState {
    local: SourceId,
    applied: u64,
    head: u64,
    batches: u64,
    rows: u64,
    phase: String,
    last_error: Option<String>,
    pending_repair: Option<PendingRepair>,
}

struct PendingRepair {
    epoch: SourceEpoch,
    through_seq: u64,
    through_chain: ChainHash,
    leaf_bits: u8,
    remaining: BTreeSet<u32>,
    leaves: Vec<u32>,
    rows: Vec<SyncRow>,
    objects: BTreeSet<eidos_domain::ObjectId>,
    bytes: u64,
    since: Instant,
}

struct Session<W> {
    ctx: Arc<SessionContext>,
    writer: W,
    peer: FleetPeer,
    peer_hello: Hello,
    key: SessionKey,
    direction: Direction,
    max_frame: usize,
    /// Bytes we may still put in flight towards the peer.
    credit: i64,
    credit_limit: i64,
    shipping: BTreeMap<SourceId, ShipState>,
    consuming: BTreeMap<SourceId, ConsumeState>,
    last_rx: Instant,
    last_ping: Option<(u64, Instant)>,
    view: Arc<Mutex<SessionView>>,
    close: Arc<AtomicBool>,
    close_notify: Arc<Notify>,
    started: Instant,
}

/// Run a session on an authenticated stream. `peer_fingerprint` is what
/// TLS proved the peer holds the key for; the roster decides what it may
/// do. Returns how the session ended.
pub async fn run_session<S>(
    ctx: Arc<SessionContext>,
    stream: S,
    peer_fingerprint: [u8; 32],
    direction: Direction,
    remote_addr: Option<String>,
    mut shutdown: watch::Receiver<bool>,
) -> SessionEnd
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut rd, mut wr) = tokio::io::split(stream);
    let max_frame = ctx.config.read().max_frame();
    let admission_max = max_frame.min(ADMISSION_MAX_FRAME_BYTES);
    let my_nonce = random_nonce();
    let my_hello = Message::Hello(Hello {
        node_id: ctx.identity.node_id,
        name: ctx.identity.name.clone(),
        platform: ctx.platform.clone(),
        role: ctx.role(),
        nonce: my_nonce,
        versions: vec![PROTOCOL_VERSION],
        features: vec![],
        max_frame_bytes: max_frame as u64,
        credit_bytes: ctx.config.read().credit_bytes(),
    });

    // The dialing side speaks first so an accepting central can tell an
    // enrollment from a session before it says anything about itself.
    if direction == Direction::Outbound {
        if let Err(e) = send(&mut wr, &my_hello, max_frame, &ctx.counters).await {
            return SessionEnd::Failed(format!("sending hello: {e}"));
        }
    }
    let first =
        match tokio::time::timeout(HELLO_TIMEOUT, wire::read_frame(&mut rd, admission_max)).await {
            Ok(Ok((msg, n))) => {
                ctx.counters.bytes_received(msg.family(), n as u64);
                msg
            }
            Ok(Err(e)) => {
                match &e {
                    wire::FrameError::TooLarge { .. } => {
                        ctx.counters.add(&ctx.counters.frames_refused_oversize, 1)
                    }
                    wire::FrameError::Malformed(_) => {
                        ctx.counters.add(&ctx.counters.frames_malformed, 1)
                    }
                    _ => {}
                }
                return SessionEnd::Failed(format!("waiting for hello: {e}"));
            }
            Err(_) => return SessionEnd::Failed("hello timed out".into()),
        };

    let peer = match ctx.catalog.fleet_peer_by_fingerprint(&peer_fingerprint) {
        Ok(peer) => peer,
        Err(e) => return SessionEnd::Failed(format!("roster lookup: {e}")),
    };
    let peer_hello = match (first, peer) {
        (
            Message::Enroll {
                secret,
                name,
                platform,
            },
            None,
        ) if direction == Direction::Inbound => {
            return enroll_peer(
                &ctx,
                &mut wr,
                max_frame,
                peer_fingerprint,
                secret,
                name,
                platform,
            )
            .await;
        }
        (Message::Hello(_), None) | (Message::Enroll { .. }, None) => {
            ctx.counters
                .add(&ctx.counters.connections_refused_unknown_peer, 1);
            let _ = send(
                &mut wr,
                &Message::Goodbye {
                    reason: "unknown peer".into(),
                },
                max_frame,
                &ctx.counters,
            )
            .await;
            return SessionEnd::UnknownPeer;
        }
        (Message::Hello(hello), Some(peer)) => {
            if peer.node_id != crate::identity::node_id_of(&peer_fingerprint) {
                let _ = send(
                    &mut wr,
                    &Message::Goodbye {
                        reason: "roster identity does not match the enrolled key".into(),
                    },
                    max_frame,
                    &ctx.counters,
                )
                .await;
                return SessionEnd::UnknownPeer;
            }
            if hello.node_id != peer.node_id {
                let _ = send(
                    &mut wr,
                    &Message::Goodbye {
                        reason: "node id does not match the enrolled key".into(),
                    },
                    max_frame,
                    &ctx.counters,
                )
                .await;
                return SessionEnd::UnknownPeer;
            }
            if !peer.enabled {
                let _ = send(
                    &mut wr,
                    &Message::Goodbye {
                        reason: "peer is disabled".into(),
                    },
                    max_frame,
                    &ctx.counters,
                )
                .await;
                return SessionEnd::Closed;
            }
            if !hello.versions.contains(&PROTOCOL_VERSION) {
                ctx.counters
                    .add(&ctx.counters.connections_refused_version, 1);
                let _ = send(
                    &mut wr,
                    &Message::Goodbye {
                        reason: format!(
                            "no common protocol version (this side speaks {PROTOCOL_VERSION})"
                        ),
                    },
                    max_frame,
                    &ctx.counters,
                )
                .await;
                return SessionEnd::Version;
            }
            if hello.max_frame_bytes < 64 * 1024 {
                let _ = send(
                    &mut wr,
                    &Message::Goodbye {
                        reason: "peer frame limit is below the protocol minimum".into(),
                    },
                    max_frame,
                    &ctx.counters,
                )
                .await;
                return SessionEnd::Version;
            }
            (hello, peer)
        }
        (Message::Goodbye { .. }, None) => return SessionEnd::UnknownPeer,
        (Message::Goodbye { .. }, Some(_)) => {
            return SessionEnd::Failed("peer closed during hello".into())
        }
        (other, _) => {
            return SessionEnd::Failed(format!("expected hello, got {}", other.kind()));
        }
    };
    let (peer_hello, peer) = peer_hello;
    if direction == Direction::Inbound {
        if let Err(e) = send(&mut wr, &my_hello, max_frame, &ctx.counters).await {
            return SessionEnd::Failed(format!("sending hello: {e}"));
        }
    }
    let key = match direction {
        Direction::Outbound => SessionKey {
            initiator: ctx.identity.node_id,
            nonce: my_nonce,
        },
        Direction::Inbound => SessionKey {
            initiator: peer.node_id,
            nonce: peer_hello.nonce,
        },
    };
    let close = Arc::new(AtomicBool::new(false));
    let close_notify = Arc::new(Notify::new());
    let credit = peer_hello
        .credit_bytes
        .min(ctx.config.read().credit_bytes())
        .min(i64::MAX as u64) as i64;
    let view = Arc::new(Mutex::new(SessionView {
        peer: peer.node_id,
        peer_name: peer.name.clone(),
        direction,
        since: UnixNanos::now(),
        remote_addr,
        last_activity_ms_ago: 0,
        credit_remaining: credit,
        sources: Vec::new(),
    }));
    if ctx
        .registry
        .register(
            peer.node_id,
            key,
            close.clone(),
            close_notify.clone(),
            view.clone(),
        )
        .is_err()
    {
        ctx.counters.add(&ctx.counters.duplicate_sessions_closed, 1);
        let _ = send(
            &mut wr,
            &Message::Goodbye {
                reason: "duplicate session".into(),
            },
            max_frame,
            &ctx.counters,
        )
        .await;
        return SessionEnd::Duplicate;
    }
    match direction {
        Direction::Outbound => ctx
            .counters
            .add(&ctx.counters.connections_established_outbound, 1),
        Direction::Inbound => ctx
            .counters
            .add(&ctx.counters.connections_established_inbound, 1),
    }
    let _ = ctx.catalog.fleet_note_peer_seen(peer.node_id, None);
    tracing::info!(peer = %peer.node_id, name = %peer.name, ?direction, "fleet session established");

    // Frames arrive through a task so a slow apply never leaves a partial
    // read behind when the loop turns to its timer.
    let (tx, mut rx) = mpsc::channel::<Result<(Message, usize), wire::FrameError>>(1);
    let reader = tokio::spawn(async move {
        loop {
            let item = wire::read_frame(&mut rd, max_frame).await;
            let stop = item.is_err();
            if tx.send(item).await.is_err() || stop {
                return;
            }
        }
    });

    let peer_max = usize::try_from(peer_hello.max_frame_bytes)
        .unwrap_or(usize::MAX)
        .min(max_frame);
    let mut session = Session {
        credit,
        credit_limit: credit,
        ctx: ctx.clone(),
        writer: wr,
        peer: peer.clone(),
        peer_hello,
        key,
        direction,
        max_frame: peer_max,
        shipping: BTreeMap::new(),
        consuming: BTreeMap::new(),
        last_rx: Instant::now(),
        last_ping: None,
        view,
        close: close.clone(),
        close_notify,
        started: Instant::now(),
    };
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let end = loop {
        tokio::select! {
            item = rx.recv() => match item {
                Some(Ok((msg, n))) => {
                    session.ctx.counters.bytes_received(msg.family(), n as u64);
                    session.last_rx = Instant::now();
                    match session.handle(msg, n).await {
                        Ok(true) => {}
                        Ok(false) => break SessionEnd::Closed,
                        Err(e) => break SessionEnd::Failed(e.to_string()),
                    }
                }
                Some(Err(wire::FrameError::Closed)) | None => break SessionEnd::Closed,
                Some(Err(e)) => {
                    match &e {
                        wire::FrameError::TooLarge { .. } => session.ctx.counters.add(&session.ctx.counters.frames_refused_oversize, 1),
                        wire::FrameError::Malformed(_) => session.ctx.counters.add(&session.ctx.counters.frames_malformed, 1),
                        _ => {}
                    }
                    // A malformed or oversized frame fails closed for this
                    // peer; nothing local is affected.
                    let _ = session.send(&Message::Goodbye { reason: e.to_string() }).await;
                    break SessionEnd::Failed(e.to_string());
                }
            },
            _ = tick.tick() => {
                if session.close.load(Ordering::Relaxed) || *shutdown.borrow() {
                    let _ = session.send(&Message::Goodbye { reason: "closing".into() }).await;
                    break if session.close.load(Ordering::Relaxed) && !*shutdown.borrow() {
                        SessionEnd::Duplicate
                    } else {
                        SessionEnd::Closed
                    };
                }
                if let Err(e) = session.tick().await {
                    break SessionEnd::Failed(e.to_string());
                }
            },
            _ = shutdown.changed() => {
                let _ = session.send(&Message::Goodbye { reason: "shutting down".into() }).await;
                break SessionEnd::Closed;
            }
            _ = session.close_notify.notified() => {
                if *shutdown.borrow() {
                    let _ = session.send(&Message::Goodbye { reason: "shutting down".into() }).await;
                    break SessionEnd::Closed;
                }
                session.ctx.counters.add(&session.ctx.counters.duplicate_sessions_closed, 1);
                let _ = session.send(&Message::Goodbye { reason: "duplicate session".into() }).await;
                break SessionEnd::Duplicate;
            }
        }
    };
    reader.abort();
    ctx.registry.unregister(peer.node_id, key);
    ctx.counters.add(&ctx.counters.disconnects, 1);
    if let SessionEnd::Failed(reason) = &end {
        let _ = ctx.catalog.fleet_note_peer_seen(peer.node_id, Some(reason));
    }
    tracing::info!(peer = %peer.node_id, ?end, "fleet session ended");
    end
}

async fn enroll_peer<W: AsyncWrite + Unpin>(
    ctx: &SessionContext,
    wr: &mut W,
    max_frame: usize,
    fingerprint: [u8; 32],
    secret: EnrollmentSecret,
    name: String,
    _platform: String,
) -> SessionEnd {
    let reject = |reason: &str| Message::EnrollRejected {
        reason: reason.into(),
    };
    if !ctx.config.read().central {
        ctx.counters
            .add(&ctx.counters.connections_refused_unknown_peer, 1);
        let _ = send(
            wr,
            &reject("this installation is not a central"),
            max_frame,
            &ctx.counters,
        )
        .await;
        return SessionEnd::UnknownPeer;
    }
    let Some(secret) = crate::identity::unhex::<32>(secret.as_str()) else {
        ctx.counters
            .add(&ctx.counters.connections_refused_unknown_peer, 1);
        let _ = send(
            wr,
            &reject("malformed invitation"),
            max_frame,
            &ctx.counters,
        )
        .await;
        return SessionEnd::UnknownPeer;
    };
    let secret = Zeroizing::new(secret);
    let node_id = crate::identity::node_id_of(&fingerprint);
    let hash = crate::InviteCode::token_hash(&secret);
    let fallback_name = sanitize_name(&name);
    let peer = FleetPeer {
        node_id,
        name: fallback_name,
        role: PeerRole::Node,
        fingerprint,
        endpoint: None,
        enabled: true,
        enrolled_at: UnixNanos::now(),
        last_seen_at: Some(UnixNanos::now()),
        last_error: None,
    };
    let redeemed = ctx.catalog.fleet_redeem_invite_and_upsert_peer(hash, &peer);
    match redeemed {
        Ok(Some(_)) => {
            ctx.counters.add(&ctx.counters.enrollments, 1);
            tracing::info!(node = %node_id, "enrolled a fleet node");
            let _ = send(
                wr,
                &Message::Enrolled {
                    node_id: ctx.identity.node_id,
                    name: ctx.identity.name.clone(),
                },
                max_frame,
                &ctx.counters,
            )
            .await;
            SessionEnd::Enrolled(node_id)
        }
        Ok(None) => {
            // Bounded and auditable: the attempt is counted and logged with
            // the fingerprint only, and nothing about the roster is said.
            ctx.counters
                .add(&ctx.counters.connections_refused_unknown_peer, 1);
            tracing::warn!(fingerprint = %crate::identity::hex(&fingerprint), "enrollment refused: invitation unknown, used, or expired");
            let _ = send(
                wr,
                &reject("invitation is not valid"),
                max_frame,
                &ctx.counters,
            )
            .await;
            SessionEnd::UnknownPeer
        }
        Err(e) => {
            let _ = send(
                wr,
                &reject("enrollment unavailable"),
                max_frame,
                &ctx.counters,
            )
            .await;
            SessionEnd::Failed(format!("enrollment: {e}"))
        }
    }
}

fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(64)
        .collect();
    if cleaned.is_empty() {
        "node".into()
    } else {
        cleaned
    }
}

fn sanitize_peer_reason(reason: &str) -> String {
    let cleaned: String = reason
        .chars()
        .filter(|c| !c.is_control())
        .take(256)
        .collect();
    if cleaned.is_empty() {
        "peer rejected the source".into()
    } else {
        cleaned
    }
}

fn random_nonce() -> u64 {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("entropy");
    u64::from_le_bytes(bytes)
}

fn replenish_credit(credit: &mut i64, limit: i64, bytes: u64) {
    let bytes = bytes.min(i64::MAX as u64) as i64;
    *credit = credit.saturating_add(bytes).min(limit);
}

async fn send<W: AsyncWrite + Unpin>(
    wr: &mut W,
    msg: &Message,
    max_frame: usize,
    counters: &FleetCounters,
) -> anyhow::Result<usize> {
    let mut bytes = wire::encode(msg)?;
    let result = write_encoded(wr, &bytes, max_frame, msg.kind()).await;
    if matches!(msg, Message::Enroll { .. }) {
        bytes.zeroize();
    }
    let n = result?;
    counters.bytes_sent(msg.family(), n as u64);
    Ok(n)
}

async fn write_encoded<W: AsyncWrite + Unpin>(
    wr: &mut W,
    bytes: &[u8],
    max_frame: usize,
    kind: &str,
) -> anyhow::Result<usize> {
    let n = tokio::time::timeout(WRITE_TIMEOUT, wire::write_frame(wr, bytes, max_frame))
        .await
        .map_err(|_| anyhow!("writing {kind} frame timed out"))??;
    Ok(n)
}

impl<W: AsyncWrite + Unpin> Session<W> {
    async fn send(&mut self, msg: &Message) -> anyhow::Result<usize> {
        send(&mut self.writer, msg, self.max_frame, &self.ctx.counters).await
    }

    fn remote_node(&self) -> RemoteNode {
        RemoteNode {
            node_id: self.peer.node_id.0,
            name: self.peer.name.clone(),
            platform: self.peer_hello.platform.clone(),
        }
    }

    /// Handle one inbound message. `Ok(false)` ends the session.
    async fn handle(&mut self, msg: Message, frame_bytes: usize) -> anyhow::Result<bool> {
        match msg {
            Message::Hello(_) => Err(anyhow!("unexpected second hello")),
            Message::Goodbye { .. } => {
                tracing::debug!(peer = %self.peer.node_id, "peer closed the session");
                Ok(false)
            }
            Message::Ping { nonce } => {
                self.send(&Message::Pong { nonce }).await?;
                Ok(true)
            }
            Message::Pong { nonce } => {
                if self.last_ping.is_some_and(|(n, _)| n == nonce) {
                    self.last_ping = None;
                }
                Ok(true)
            }
            Message::Enroll { .. } | Message::Enrolled { .. } | Message::EnrollRejected { .. } => {
                Err(anyhow!("enrollment message inside an established session"))
            }
            // ----- consumer role ---------------------------------------
            Message::Offer {
                descriptor,
                epoch,
                head_seq,
                head_chain,
                compacted_through,
                image_version,
            } => {
                self.on_offer(
                    descriptor,
                    epoch,
                    head_seq,
                    head_chain,
                    compacted_through,
                    image_version,
                )
                .await?;
                Ok(true)
            }
            Message::Batch(batch) => {
                self.on_batch(batch).await?;
                Ok(true)
            }
            Message::RepairOffer {
                source,
                epoch,
                through_seq,
                through_chain,
                leaf_bits,
                leaf_hashes,
            } => {
                self.on_repair_offer(
                    source,
                    epoch,
                    through_seq,
                    through_chain,
                    leaf_bits,
                    leaf_hashes,
                )
                .await?;
                Ok(true)
            }
            Message::RepairRows {
                source,
                epoch,
                through_seq,
                through_chain,
                leaf_bits,
                leaves,
                rows,
                final_part,
            } => {
                self.on_repair_rows(
                    source,
                    epoch,
                    through_seq,
                    through_chain,
                    leaf_bits,
                    leaves,
                    rows,
                    final_part,
                    frame_bytes,
                )
                .await?;
                Ok(true)
            }
            // ----- shipper role ----------------------------------------
            Message::Resume {
                source,
                epoch,
                after_seq,
                requires_repair,
            } => {
                self.on_resume(source, epoch, after_seq, requires_repair)
                    .await?;
                Ok(true)
            }
            Message::FullResync { source, epoch } => {
                self.on_full_resync(source, epoch).await?;
                Ok(true)
            }
            Message::Ack {
                source,
                epoch,
                through_seq,
            } => {
                self.on_ack(source, epoch, through_seq).await?;
                Ok(true)
            }
            Message::Rejected { source, reason } => {
                self.on_rejected(source, reason);
                Ok(true)
            }
            Message::RepairRequest {
                source,
                epoch,
                through_seq,
                through_chain,
                leaf_bits,
                leaves,
            } => {
                self.on_repair_request(
                    source,
                    epoch,
                    through_seq,
                    through_chain,
                    leaf_bits,
                    leaves,
                )
                .await?;
                Ok(true)
            }
        }
    }

    // ----- consumer role -------------------------------------------------

    async fn on_offer(
        &mut self,
        descriptor: RemoteSourceDescriptor,
        epoch: SourceEpoch,
        head_seq: u64,
        head_chain: ChainHash,
        compacted_through: u64,
        image_version: u32,
    ) -> anyhow::Result<()> {
        self.ctx.counters.add(&self.ctx.counters.offers_received, 1);
        let remote = descriptor.remote_source_id;
        if !self.ctx.config.read().central {
            // A node consumes nothing; say so instead of silently ignoring.
            self.ctx.counters.add(&self.ctx.counters.rejections_sent, 1);
            self.send(&Message::Rejected {
                source: remote,
                reason: "this side is not a central".into(),
            })
            .await?;
            return Ok(());
        }
        if image_version != SYNC_ROW_IMAGE_VERSION {
            self.ctx.counters.add(&self.ctx.counters.rejections_sent, 1);
            self.send(&Message::Rejected {
                source: remote,
                reason: format!(
                    "row image version {image_version} is not supported (this side applies {SYNC_ROW_IMAGE_VERSION})"
                ),
            })
            .await?;
            return Ok(());
        }
        let catalog = self.ctx.catalog.clone();
        let node = self.remote_node();
        let outcome =
            tokio::task::spawn_blocking(move || -> anyhow::Result<(SourceId, HelloOutcome)> {
                let state = catalog.replica_ensure_source(&node, &descriptor, epoch)?;
                let outcome = catalog.replica_admit_hello(
                    state.source_id,
                    epoch,
                    head_seq,
                    head_chain,
                    compacted_through,
                )?;
                Ok((state.source_id, outcome))
            })
            .await
            .context("replica task")??;
        let (local, outcome) = outcome;
        let entry = self
            .consuming
            .entry(remote)
            .or_insert_with(|| ConsumeState {
                local,
                applied: 0,
                head: head_seq,
                batches: 0,
                rows: 0,
                phase: "offered".into(),
                last_error: None,
                pending_repair: None,
            });
        entry.head = head_seq;
        entry.pending_repair = None;
        let reply = match outcome {
            HelloOutcome::Resume {
                epoch,
                after_seq,
                requires_repair,
            } => {
                entry.applied = after_seq;
                entry.phase = if requires_repair {
                    "awaiting repair offer".into()
                } else {
                    "resuming".into()
                };
                Message::Resume {
                    source: remote,
                    epoch,
                    after_seq,
                    requires_repair,
                }
            }
            HelloOutcome::FullResync { epoch } => {
                self.ctx.counters.add(&self.ctx.counters.full_resyncs, 1);
                entry.phase = "full resync".into();
                Message::FullResync {
                    source: remote,
                    epoch,
                }
            }
            HelloOutcome::Rejected { reason } => {
                self.ctx.counters.add(&self.ctx.counters.fences, 1);
                self.ctx.counters.add(&self.ctx.counters.rejections_sent, 1);
                entry.phase = "fenced".into();
                entry.last_error = Some(reason.clone());
                tracing::warn!(peer = %self.peer.node_id, source = remote.0, %reason, "fenced a replicated source");
                Message::Rejected {
                    source: remote,
                    reason,
                }
            }
        };
        self.send(&reply).await?;
        Ok(())
    }

    async fn on_batch(&mut self, batch: eidos_catalog::sync::SyncBatch) -> anyhow::Result<()> {
        let remote = batch.source_id;
        if !self.ctx.config.read().central {
            self.ctx.counters.add(&self.ctx.counters.rejections_sent, 1);
            self.send(&Message::Rejected {
                source: remote,
                reason: "this side is not a central".into(),
            })
            .await?;
            return Ok(());
        }
        if self
            .consuming
            .get(&remote)
            .is_some_and(|state| state.pending_repair.is_some())
        {
            self.ctx.counters.add(&self.ctx.counters.rejections_sent, 1);
            self.send(&Message::Rejected {
                source: remote,
                reason: "repair is still in progress".into(),
            })
            .await?;
            return Ok(());
        }
        let Some(local) = self.consuming.get(&remote).map(|c| c.local) else {
            self.ctx.counters.add(&self.ctx.counters.rejections_sent, 1);
            self.send(&Message::Rejected {
                source: remote,
                reason: "hello required before batch".into(),
            })
            .await?;
            return Ok(());
        };
        let catalog = self.ctx.catalog.clone();
        let epoch = batch.epoch.to_source_epoch();
        let rows = batch.rows.len() as u64;
        let started = Instant::now();
        let outcome =
            tokio::task::spawn_blocking(move || catalog.replica_apply_batch(local, &batch))
                .await
                .context("apply task")??;
        self.ctx.counters.add(
            &self.ctx.counters.apply_ms_total,
            started.elapsed().as_millis() as u64,
        );
        let state = self.consuming.get_mut(&remote).expect("mapped");
        let reply = match outcome {
            BatchOutcome::Applied {
                through_seq,
                rows: applied,
                retired_rows,
            } => {
                self.ctx.counters.add(&self.ctx.counters.batches_applied, 1);
                self.ctx
                    .counters
                    .add(&self.ctx.counters.rows_applied, applied);
                self.ctx.counters.add(&self.ctx.counters.acks_sent, 1);
                state.applied = through_seq;
                state.batches += 1;
                state.rows += rows;
                state.phase = if retired_rows > 0 {
                    "applied; previous epoch retired".into()
                } else {
                    "applying".into()
                };
                Message::Ack {
                    source: remote,
                    epoch,
                    through_seq,
                }
            }
            BatchOutcome::AlreadyApplied { applied_seq } => {
                self.ctx
                    .counters
                    .add(&self.ctx.counters.duplicates_acknowledged, 1);
                self.ctx.counters.add(&self.ctx.counters.acks_sent, 1);
                Message::Ack {
                    source: remote,
                    epoch,
                    through_seq: applied_seq,
                }
            }
            BatchOutcome::Stale { applied_seq } => {
                self.ctx.counters.add(&self.ctx.counters.stale_batches, 1);
                Message::Resume {
                    source: remote,
                    epoch,
                    after_seq: applied_seq,
                    requires_repair: false,
                }
            }
            BatchOutcome::FullResyncRequired { epoch } => {
                self.ctx.counters.add(&self.ctx.counters.full_resyncs, 1);
                Message::FullResync {
                    source: remote,
                    epoch,
                }
            }
            BatchOutcome::Rejected { reason } => {
                self.ctx.counters.add(&self.ctx.counters.fences, 1);
                self.ctx.counters.add(&self.ctx.counters.rejections_sent, 1);
                state.phase = "fenced".into();
                state.last_error = Some(reason.clone());
                tracing::warn!(peer = %self.peer.node_id, source = remote.0, %reason, "rejected a batch");
                Message::Rejected {
                    source: remote,
                    reason,
                }
            }
        };
        self.send(&reply).await?;
        Ok(())
    }

    async fn on_repair_offer(
        &mut self,
        source: SourceId,
        epoch: SourceEpoch,
        through_seq: u64,
        through_chain: ChainHash,
        leaf_bits: u8,
        leaf_hashes: Vec<[u8; 32]>,
    ) -> anyhow::Result<()> {
        if !self.ctx.config.read().central {
            self.ctx.counters.add(&self.ctx.counters.rejections_sent, 1);
            self.send(&Message::Rejected {
                source,
                reason: "this side is not a central".into(),
            })
            .await?;
            return Ok(());
        }
        let Some(local) = self.consuming.get(&source).map(|c| c.local) else {
            self.send(&Message::Rejected {
                source,
                reason: "hello required before repair".into(),
            })
            .await?;
            return Ok(());
        };
        let catalog = self.ctx.catalog.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            catalog.replica_repair_offer(
                local,
                epoch,
                through_seq,
                through_chain,
                leaf_bits,
                &leaf_hashes,
            )
        })
        .await
        .context("repair task")??;
        let reply = match outcome {
            RepairOfferOutcome::Request { leaf_bits, leaves } => {
                if let Some(state) = self.consuming.get_mut(&source) {
                    state.phase = format!("repairing {} leaves", leaves.len());
                    state.pending_repair = Some(PendingRepair {
                        epoch,
                        through_seq,
                        through_chain,
                        leaf_bits,
                        remaining: leaves.iter().copied().collect(),
                        leaves: Vec::new(),
                        rows: Vec::new(),
                        objects: BTreeSet::new(),
                        bytes: 0,
                        since: Instant::now(),
                    });
                }
                Message::RepairRequest {
                    source,
                    epoch,
                    through_seq,
                    through_chain,
                    leaf_bits,
                    leaves,
                }
            }
            RepairOfferOutcome::Rejected { reason } => {
                self.ctx.counters.add(&self.ctx.counters.rejections_sent, 1);
                if let Some(state) = self.consuming.get_mut(&source) {
                    state.phase = "fenced".into();
                    state.last_error = Some(reason.clone());
                    state.pending_repair = None;
                }
                Message::Rejected { source, reason }
            }
        };
        self.send(&reply).await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn on_repair_rows(
        &mut self,
        source: SourceId,
        epoch: SourceEpoch,
        through_seq: u64,
        through_chain: ChainHash,
        leaf_bits: u8,
        leaves: Vec<u32>,
        rows: Vec<SyncRow>,
        final_part: bool,
        frame_bytes: usize,
    ) -> anyhow::Result<()> {
        if !self.ctx.config.read().central {
            self.ctx.counters.add(&self.ctx.counters.rejections_sent, 1);
            self.send(&Message::Rejected {
                source,
                reason: "this side is not a central".into(),
            })
            .await?;
            return Ok(());
        }
        if !self.consuming.contains_key(&source) {
            self.send(&Message::Rejected {
                source,
                reason: "hello required before repair".into(),
            })
            .await?;
            return Ok(());
        }

        // Repair parts are staged in bounded session memory and applied in
        // one transaction only after the complete requested leaf set has
        // arrived. A disconnect between parts therefore cannot advance the
        // durable cursor past leaves that were never installed.
        let memory_limit = self.ctx.config.read().credit_bytes();
        let staged: Result<Option<(SourceId, PendingRepair)>, String> = {
            let state = self.consuming.get_mut(&source).expect("mapped");
            if let Some(mut pending) = state.pending_repair.take() {
                let mut invalid = None;
                if pending.epoch != epoch
                    || pending.through_seq != through_seq
                    || pending.through_chain != through_chain
                    || pending.leaf_bits != leaf_bits
                {
                    invalid = Some("repair rows do not match the outstanding request".to_string());
                }
                let part_leaves: BTreeSet<u32> = leaves.iter().copied().collect();
                if invalid.is_none()
                    && (part_leaves.len() != leaves.len()
                        || !part_leaves.is_subset(&pending.remaining)
                        || (part_leaves.is_empty()
                            && !(final_part && pending.remaining.is_empty())))
                {
                    invalid =
                        Some("repair part has duplicate, unexpected, or empty leaves".to_string());
                }
                if invalid.is_none()
                    && rows.iter().any(|row| {
                        !part_leaves.contains(&leaf_index(leaf_bits, row.object))
                            || !pending.objects.insert(row.object)
                    })
                {
                    invalid = Some("repair part has an unexpected or duplicate row".to_string());
                }
                let bytes = pending.bytes.checked_add(frame_bytes as u64);
                if invalid.is_none() && bytes.is_none_or(|bytes| bytes > memory_limit) {
                    invalid = Some(format!(
                        "repair response exceeds the {memory_limit}-byte staging limit"
                    ));
                }
                if let Some(reason) = invalid {
                    state.phase = "fenced".into();
                    state.last_error = Some(reason.clone());
                    Err(reason)
                } else {
                    pending.bytes = bytes.expect("validated");
                    for leaf in &part_leaves {
                        pending.remaining.remove(leaf);
                    }
                    pending.leaves.extend(leaves);
                    pending.rows.extend(rows);
                    if final_part != pending.remaining.is_empty() {
                        let reason = "repair final marker does not complete the requested leaves";
                        state.phase = "fenced".into();
                        state.last_error = Some(reason.into());
                        Err(reason.into())
                    } else if final_part {
                        Ok(Some((state.local, pending)))
                    } else {
                        state.phase = format!("repairing {} leaves", pending.remaining.len());
                        state.pending_repair = Some(pending);
                        Ok(None)
                    }
                }
            } else {
                let reason = "repair rows were not requested".to_string();
                state.phase = "fenced".into();
                state.last_error = Some(reason.clone());
                Err(reason)
            }
        };
        let Some((local, pending)) = (match staged {
            Ok(staged) => staged,
            Err(reason) => {
                self.ctx.counters.add(&self.ctx.counters.rejections_sent, 1);
                self.send(&Message::Rejected { source, reason }).await?;
                return Ok(());
            }
        }) else {
            return Ok(());
        };
        let catalog = self.ctx.catalog.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            catalog.replica_apply_repair(
                local,
                pending.epoch,
                pending.through_seq,
                pending.through_chain,
                pending.leaf_bits,
                &pending.leaves,
                &pending.rows,
            )
        })
        .await
        .context("repair task")??;
        let reply = match outcome {
            RepairOutcome::Applied {
                through_seq,
                replaced,
                removed,
            } => {
                self.ctx.counters.add(&self.ctx.counters.repairs_applied, 1);
                self.ctx
                    .counters
                    .add(&self.ctx.counters.repair_rows_applied, replaced + removed);
                if let Some(state) = self.consuming.get_mut(&source) {
                    state.applied = through_seq;
                    state.phase = "repaired".into();
                }
                self.ctx.counters.add(&self.ctx.counters.acks_sent, 1);
                Message::Ack {
                    source,
                    epoch,
                    through_seq,
                }
            }
            RepairOutcome::Rejected { reason } => {
                self.ctx.counters.add(&self.ctx.counters.rejections_sent, 1);
                if let Some(state) = self.consuming.get_mut(&source) {
                    state.phase = "fenced".into();
                    state.last_error = Some(reason.clone());
                }
                Message::Rejected { source, reason }
            }
        };
        self.send(&reply).await?;
        Ok(())
    }

    // ----- shipper role --------------------------------------------------

    async fn on_resume(
        &mut self,
        source: SourceId,
        epoch: SourceEpoch,
        after_seq: u64,
        requires_repair: bool,
    ) -> anyhow::Result<()> {
        let Some(state) = self.shipping.get_mut(&source) else {
            return Ok(());
        };
        if state.epoch != epoch {
            return Ok(());
        }
        let in_flight = match state.phase {
            ShipPhase::InFlight { bytes, .. } => bytes,
            _ => 0,
        };
        if after_seq > state.head {
            // The peer holds history we no longer have: a rewind. The next
            // offer reports our head and the peer fences it; do not send a
            // batch it would refuse.
            state.phase = ShipPhase::Fenced {
                reason: format!("peer cursor {after_seq} is beyond our head {}", state.head),
                since: Instant::now(),
            };
            replenish_credit(&mut self.credit, self.credit_limit, in_flight);
            return Ok(());
        }
        if after_seq < state.cursor {
            state.phase = ShipPhase::Fenced {
                reason: format!("peer cursor regressed from {} to {after_seq}", state.cursor),
                since: Instant::now(),
            };
            replenish_credit(&mut self.credit, self.credit_limit, in_flight);
            return Ok(());
        }
        if after_seq == state.cursor && matches!(state.phase, ShipPhase::InFlight { .. }) {
            // A delayed duplicate Resume must not release the credit owned
            // by a newer batch.
            return Ok(());
        }
        state.cursor = after_seq;
        replenish_credit(&mut self.credit, self.credit_limit, in_flight);
        self.acknowledge_durably(source, after_seq).await?;
        if requires_repair {
            self.ctx.counters.add(&self.ctx.counters.repairs_offered, 1);
            self.send_repair_offer(source).await?;
        } else {
            if let Some(state) = self.shipping.get_mut(&source) {
                state.phase = ShipPhase::Idle;
            }
            self.ship(source).await?;
        }
        Ok(())
    }

    async fn on_full_resync(&mut self, source: SourceId, epoch: SourceEpoch) -> anyhow::Result<()> {
        let Some(state) = self.shipping.get_mut(&source) else {
            return Ok(());
        };
        if state.epoch != epoch {
            return Ok(());
        }
        let in_flight = match state.phase {
            ShipPhase::InFlight { bytes, .. } => bytes,
            _ => 0,
        };
        if state.cursor != 0 {
            state.phase = ShipPhase::Fenced {
                reason: format!(
                    "full resync would regress durable cursor {} in the same epoch",
                    state.cursor
                ),
                since: Instant::now(),
            };
            replenish_credit(&mut self.credit, self.credit_limit, in_flight);
            return Ok(());
        }
        replenish_credit(&mut self.credit, self.credit_limit, in_flight);
        state.cursor = 0;
        state.phase = ShipPhase::Idle;
        self.ship(source).await
    }

    async fn on_ack(
        &mut self,
        source: SourceId,
        epoch: SourceEpoch,
        through_seq: u64,
    ) -> anyhow::Result<()> {
        self.ctx.counters.add(&self.ctx.counters.acks_received, 1);
        let Some(state) = self.shipping.get_mut(&source) else {
            return Ok(());
        };
        if state.epoch != epoch {
            return Ok(());
        }
        if through_seq > state.head {
            let bytes = match state.phase {
                ShipPhase::InFlight { bytes, .. } => bytes,
                _ => 0,
            };
            state.phase = ShipPhase::Fenced {
                reason: format!(
                    "peer acknowledged {through_seq} beyond our head {}",
                    state.head
                ),
                since: Instant::now(),
            };
            replenish_credit(&mut self.credit, self.credit_limit, bytes);
            return Ok(());
        }
        let bytes = match state.phase {
            ShipPhase::InFlight {
                through_seq: expected,
                bytes,
                ..
            } if through_seq == expected => bytes,
            ShipPhase::InFlight { .. } if through_seq <= state.cursor => return Ok(()),
            ShipPhase::InFlight {
                through_seq: expected,
                bytes,
                ..
            } => {
                state.phase = ShipPhase::Fenced {
                    reason: format!(
                        "peer acknowledged {through_seq}, but batch through {expected} is in flight"
                    ),
                    since: Instant::now(),
                };
                replenish_credit(&mut self.credit, self.credit_limit, bytes);
                return Ok(());
            }
            _ if through_seq <= state.cursor => return Ok(()),
            _ => {
                state.phase = ShipPhase::Fenced {
                    reason: format!("unexpected acknowledgement through {through_seq}"),
                    since: Instant::now(),
                };
                return Ok(());
            }
        };
        replenish_credit(&mut self.credit, self.credit_limit, bytes);
        state.cursor = through_seq;
        state.phase = ShipPhase::Idle;
        self.acknowledge_durably(source, through_seq).await?;
        self.ship(source).await
    }

    fn on_rejected(&mut self, source: SourceId, reason: String) {
        self.ctx
            .counters
            .add(&self.ctx.counters.rejections_received, 1);
        if let Some(state) = self.shipping.get_mut(&source) {
            let reason = sanitize_peer_reason(&reason);
            let bytes = match state.phase {
                ShipPhase::InFlight { bytes, .. } => bytes,
                _ => 0,
            };
            replenish_credit(&mut self.credit, self.credit_limit, bytes);
            tracing::warn!(peer = %self.peer.node_id, source = source.0, "peer rejected a source");
            state.last_error = Some(reason.clone());
            state.phase = ShipPhase::Fenced {
                reason,
                since: Instant::now(),
            };
        }
    }

    async fn on_repair_request(
        &mut self,
        source: SourceId,
        epoch: SourceEpoch,
        through_seq: u64,
        through_chain: ChainHash,
        leaf_bits: u8,
        leaves: Vec<u32>,
    ) -> anyhow::Result<()> {
        let Some(state) = self.shipping.get(&source) else {
            return Err(anyhow!("repair requested for an unavailable source"));
        };
        let request_matches = matches!(
            state.phase,
            ShipPhase::Repairing {
                through_seq: offered_seq,
                through_chain: offered_chain,
                leaf_bits: offered_bits,
                ..
            } if offered_seq == through_seq
                && offered_chain == through_chain
                && offered_bits == leaf_bits
        );
        let wanted: BTreeSet<u32> = leaves.iter().copied().collect();
        if state.epoch != epoch
            || leaf_bits > MAX_FLEET_LEAF_BITS
            || !request_matches
            || wanted.len() != leaves.len()
            || wanted.iter().any(|leaf| *leaf >= (1u32 << leaf_bits))
        {
            return Err(anyhow!("invalid or unsolicited repair request"));
        }
        if self.credit <= 0 {
            return Err(anyhow!("repair requested without remaining credit"));
        }
        let catalog = self.ctx.catalog.clone();
        let batch_bytes = self.ctx.config.read().batch_bytes();
        let repair_budget = self.credit.max(0) as usize;
        // Rows of the requested leaves, grouped so each part stays under
        // the batch limit; a part's leaves are complete within it, since
        // absence from a requested leaf is authoritative.
        let parts = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<RepairParts>> {
            let (state, entries) = catalog.sync_ledger_entries(source)?;
            if state.epoch.to_source_epoch() != epoch
                || state.head_seq != through_seq
                || state.head_chain != through_chain
            {
                // A request for a head we no longer have describes another
                // history; the next offer restarts the exchange.
                return Ok(None);
            }
            let mut by_leaf: BTreeMap<u32, Vec<eidos_domain::ObjectId>> = BTreeMap::new();
            for leaf in &wanted {
                by_leaf.entry(*leaf).or_default();
            }
            for entry in entries {
                let leaf = leaf_index(leaf_bits, entry.object);
                if wanted.contains(&leaf) {
                    by_leaf.entry(leaf).or_default().push(entry.object);
                }
            }
            let mut parts: Vec<(Vec<u32>, Vec<SyncRow>)> = Vec::new();
            let mut current: (Vec<u32>, Vec<SyncRow>, usize) = (Vec::new(), Vec::new(), 0);
            let mut total_size = 0usize;
            for (leaf, objects) in by_leaf {
                let (row_state, rows) = catalog.sync_rows_for_objects(source, &objects)?;
                if row_state.epoch.to_source_epoch() != epoch
                    || row_state.head_seq != through_seq
                    || row_state.head_chain != through_chain
                {
                    return Ok(None);
                }
                let size: usize = rows
                    .iter()
                    .map(|r| serde_json::to_vec(r).map(|v| v.len()).unwrap_or(0))
                    .sum();
                total_size = total_size
                    .checked_add(size)
                    .ok_or_else(|| anyhow!("repair response byte count overflow"))?;
                if total_size > repair_budget {
                    return Err(anyhow!(
                        "repair rows exceed the {repair_budget}-byte remaining credit"
                    ));
                }
                if !current.0.is_empty() && current.2 + size > batch_bytes {
                    parts.push((
                        std::mem::take(&mut current.0),
                        std::mem::take(&mut current.1),
                    ));
                    current.2 = 0;
                }
                current.0.push(leaf);
                current.1.extend(rows);
                current.2 += size;
            }
            parts.push((current.0, current.1));
            Ok(Some(parts))
        })
        .await
        .context("repair rows task")?;
        let parts = parts?;
        let Some(parts) = parts else {
            if let Some(state) = self.shipping.get_mut(&source) {
                state.phase = ShipPhase::Unoffered;
            }
            return Ok(());
        };
        let count = parts.len();
        let mut frames = Vec::with_capacity(count);
        let mut total_bytes = 0u64;
        for (i, (leaves, rows)) in parts.into_iter().enumerate() {
            let message = Message::RepairRows {
                source,
                epoch,
                through_seq,
                through_chain,
                leaf_bits,
                leaves,
                rows,
                final_part: i + 1 == count,
            };
            let bytes = wire::encode(&message)?;
            if bytes.len() > self.max_frame {
                return Err(anyhow!(
                    "repair part of {} bytes exceeds the {}-byte frame limit",
                    bytes.len(),
                    self.max_frame
                ));
            }
            total_bytes = total_bytes
                .checked_add(bytes.len() as u64 + 4)
                .ok_or_else(|| anyhow!("repair response byte count overflow"))?;
            frames.push(bytes);
        }
        if total_bytes > self.credit.max(0) as u64 {
            return Err(anyhow!(
                "repair response needs {total_bytes} bytes but only {} bytes of credit remain",
                self.credit.max(0)
            ));
        }
        for bytes in frames {
            let n = write_encoded(&mut self.writer, &bytes, self.max_frame, "repair_rows").await?;
            self.ctx.counters.bytes_sent(Family::Repair, n as u64);
        }
        self.credit -= total_bytes as i64;
        if let Some(state) = self.shipping.get_mut(&source) {
            state.phase = ShipPhase::InFlight {
                through_seq,
                bytes: total_bytes,
                since: Instant::now(),
            };
        }
        Ok(())
    }

    async fn acknowledge_durably(
        &mut self,
        source: SourceId,
        through_seq: u64,
    ) -> anyhow::Result<()> {
        let catalog = self.ctx.catalog.clone();
        let consumer = self.peer.node_id.0;
        let Some(epoch) = self.shipping.get(&source).map(|s| s.epoch) else {
            return Ok(());
        };
        let epoch = eidos_catalog::sync::SyncEpoch::from_source_epoch(epoch);
        let result = tokio::task::spawn_blocking(move || {
            catalog.sync_acknowledge(source, epoch, consumer, through_seq)
        })
        .await
        .context("acknowledge task")?;
        if let Err(e) = result {
            // An acknowledgement for a retired epoch or beyond the head is
            // stale, not fatal: the next offer re-establishes the cursor.
            tracing::debug!(source = source.0, error = %e, "ignored a stale acknowledgement");
        }
        Ok(())
    }

    async fn send_repair_offer(&mut self, source: SourceId) -> anyhow::Result<()> {
        if self.credit <= 0 {
            if let Some(ship) = self.shipping.get_mut(&source) {
                ship.phase = ShipPhase::Offered {
                    since: Instant::now(),
                };
            }
            return Ok(());
        }
        let catalog = self.ctx.catalog.clone();
        let configured = self.ctx.config.read().repair_leaf_bits;
        let (state, leaf_bits, hashes) =
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                let (state, entries) = catalog.sync_ledger_entries(source)?;
                let leaf_bits = configured
                    .unwrap_or(if (entries.len() as u64) < SMALL_SOURCE_ROWS {
                        MIN_LEAF_BITS
                    } else {
                        eidos_sync::merkle::MIN_FLEET_LEAF_BITS
                    })
                    .clamp(MIN_LEAF_BITS, MAX_FLEET_LEAF_BITS);
                let tree = MerkleTree::with_leaf_bits(
                    leaf_bits,
                    entries
                        .iter()
                        .map(|e| record_digest(e.object, e.generation, e.deleted)),
                );
                Ok((state, leaf_bits, tree.leaf_hashes()))
            })
            .await
            .context("merkle task")??;
        let epoch = state.epoch.to_source_epoch();
        if let Some(ship) = self.shipping.get_mut(&source) {
            ship.phase = ShipPhase::Repairing {
                through_seq: state.head_seq,
                through_chain: state.head_chain,
                leaf_bits,
                since: Instant::now(),
            };
            ship.head = state.head_seq;
            ship.epoch = epoch;
        }
        self.send(&Message::RepairOffer {
            source,
            epoch,
            through_seq: state.head_seq,
            through_chain: state.head_chain,
            leaf_bits,
            leaf_hashes: hashes,
        })
        .await?;
        Ok(())
    }

    /// Ship the next batch of `source` if it is idle, behind, and credit
    /// allows. Batches are cut by rows and by encoded bytes: one that
    /// encodes over the limit is halved and materialized again.
    async fn ship(&mut self, source: SourceId) -> anyhow::Result<()> {
        let Some(state) = self.shipping.get(&source) else {
            return Ok(());
        };
        if !matches!(state.phase, ShipPhase::Idle) || state.cursor >= state.head {
            return Ok(());
        }
        let (mut rows, bytes_cap) = {
            let cfg = self.ctx.config.read();
            (cfg.batch_rows.max(1), cfg.batch_bytes())
        };
        if self.credit <= 0 {
            return Ok(());
        }
        let cursor = state.cursor;
        let epoch = state.epoch;
        let catalog = self.ctx.catalog.clone();
        let started = Instant::now();
        let encoded =
            tokio::task::spawn_blocking(move || -> anyhow::Result<Option<EncodedBatch>> {
                loop {
                    let batch = match catalog.sync_rows_after(source, cursor, rows) {
                        Ok(b) => b,
                        Err(eidos_catalog::CatalogError::InvalidState(reason)) => {
                            return Err(anyhow!(reason));
                        }
                        Err(e) => return Err(e.into()),
                    };
                    if batch.epoch.to_source_epoch() != epoch {
                        return Ok(None);
                    }
                    if batch.rows.is_empty() {
                        return Ok(None);
                    }
                    let msg = Message::Batch(batch);
                    let bytes = wire::encode(&msg)?;
                    let Message::Batch(batch) = msg else {
                        unreachable!()
                    };
                    if bytes.len() > bytes_cap && rows > 1 {
                        rows = (rows / 2).max(1);
                        continue;
                    }
                    if bytes.len() > bytes_cap {
                        return Err(anyhow!(
                            "one-row batch of {} bytes exceeds the {bytes_cap}-byte batch limit",
                            bytes.len()
                        ));
                    }
                    return Ok(Some((
                        bytes,
                        batch.through_seq,
                        batch.rows.len() as u64,
                        batch.head_seq,
                    )));
                }
            })
            .await
            .context("materialize task")?;
        self.ctx.counters.add(
            &self.ctx.counters.materialize_ms_total,
            started.elapsed().as_millis() as u64,
        );
        let state = self.shipping.get_mut(&source).expect("still shipping");
        match encoded {
            Ok(Some((bytes, through_seq, row_count, head))) => {
                state.head = head;
                let wire_bytes = bytes.len() as u64 + 4;
                if wire_bytes > self.credit.max(0) as u64 {
                    return Ok(());
                }
                let n = write_encoded(&mut self.writer, &bytes, self.max_frame, "batch").await?;
                self.ctx.counters.bytes_sent(Family::Catalog, n as u64);
                self.ctx.counters.add(&self.ctx.counters.batches_sent, 1);
                self.ctx
                    .counters
                    .add(&self.ctx.counters.rows_shipped, row_count);
                self.credit -= n as i64;
                state.phase = ShipPhase::InFlight {
                    through_seq,
                    bytes: n as u64,
                    since: Instant::now(),
                };
                state.batches += 1;
                state.rows += row_count;
            }
            Ok(None) => {
                // The epoch moved under us: re-offer on the next tick.
                state.phase = ShipPhase::Unoffered;
            }
            Err(e) => {
                state.last_error = Some(e.to_string());
                state.phase = ShipPhase::Fenced {
                    reason: e.to_string(),
                    since: Instant::now(),
                };
            }
        }
        Ok(())
    }

    /// Refresh the set of shippable sources, offer new ones, retry fenced
    /// ones after their backoff, keep the peer alive, and publish the view.
    async fn tick(&mut self) -> anyhow::Result<()> {
        let catalog = self.ctx.catalog.clone();
        let expected = self.peer.clone();
        let admitted = tokio::task::spawn_blocking(move || {
            catalog.fleet_peer(expected.node_id).map(|peer| {
                peer.is_some_and(|peer| {
                    peer.enabled
                        && peer.fingerprint == expected.fingerprint
                        && peer.role == expected.role
                })
            })
        })
        .await
        .context("roster task")??;
        if !admitted {
            return Err(anyhow!("peer is no longer admitted by the roster"));
        }
        if let Some((source, phase)) = self.shipping.iter().find_map(|(source, state)| {
            let (name, since) = match &state.phase {
                ShipPhase::Offered { since } => ("offer", since),
                ShipPhase::InFlight { since, .. } => ("acknowledgement", since),
                ShipPhase::Repairing { since, .. } => ("repair request", since),
                _ => return None,
            };
            (since.elapsed() > PROGRESS_TIMEOUT).then_some((*source, name))
        }) {
            return Err(anyhow!(
                "timed out waiting for {phase} progress on source {source}"
            ));
        }
        if let Some(source) = self.consuming.iter().find_map(|(source, state)| {
            state
                .pending_repair
                .as_ref()
                .filter(|repair| repair.since.elapsed() > PROGRESS_TIMEOUT)
                .map(|_| *source)
        }) {
            return Err(anyhow!(
                "timed out waiting for repair rows on source {source}"
            ));
        }

        // Keepalive with a bounded deadline.
        let idle = self.last_rx.elapsed();
        match self.last_ping {
            Some((_, sent)) if sent.elapsed() > IDLE_PING => {
                return Err(anyhow!("peer silent for {}s", (idle.as_secs())));
            }
            None if idle > IDLE_PING => {
                let nonce = random_nonce();
                self.last_ping = Some((nonce, Instant::now()));
                self.send(&Message::Ping { nonce }).await?;
            }
            _ => {}
        }

        // Shipper: what should be offered to this peer right now.
        let catalog = self.ctx.catalog.clone();
        let peer_role = self.peer.role;
        let consumer = self.peer.node_id.0;
        let shippable = tokio::task::spawn_blocking(
            move || -> anyhow::Result<Vec<(SourceId, eidos_catalog::sync::SyncSourceState, u64)>> {
                // Only a central consumes; offering sources to a node would
                // only draw rejections.
                if peer_role != PeerRole::Central {
                    return Ok(Vec::new());
                }
                let mut out = Vec::new();
                for s in catalog.list_sources()? {
                    if s.kind == SourceKind::Remote
                        || s.state == SourceState::Retired
                        || s.published_generation.is_none()
                        || s.sync_policy != SyncPolicy::Inherit
                    {
                        continue;
                    }
                    if let Some(state) = catalog.sync_source(s.id)? {
                        if state.ready {
                            let cursor = catalog
                                .sync_consumers(s.id)?
                                .into_iter()
                                .find(|c| c.consumer_id == consumer)
                                .map(|c| c.watermark)
                                .unwrap_or(0);
                            out.push((s.id, state, cursor));
                        }
                    }
                }
                Ok(out)
            },
        )
        .await
        .context("source scan task")??;
        let wanted: BTreeSet<SourceId> = shippable.iter().map(|(id, _, _)| *id).collect();
        let removed_credit: u64 = self
            .shipping
            .iter()
            .filter(|(id, _)| !wanted.contains(id))
            .filter_map(|(_, state)| match state.phase {
                ShipPhase::InFlight { bytes, .. } => Some(bytes),
                _ => None,
            })
            .sum();
        self.shipping.retain(|id, _| wanted.contains(id));
        replenish_credit(&mut self.credit, self.credit_limit, removed_credit);
        let mut to_offer: Vec<(SourceId, eidos_catalog::sync::SyncSourceState)> = Vec::new();
        let mut to_ship: Vec<SourceId> = Vec::new();
        for (id, state, durable_cursor) in shippable {
            let epoch = state.epoch.to_source_epoch();
            match self.shipping.get_mut(&id) {
                None => {
                    self.shipping.insert(
                        id,
                        ShipState {
                            epoch,
                            cursor: durable_cursor,
                            head: state.head_seq,
                            phase: ShipPhase::Unoffered,
                            batches: 0,
                            rows: 0,
                            last_error: None,
                        },
                    );
                    to_offer.push((id, state));
                }
                Some(ship) => {
                    ship.head = state.head_seq;
                    if ship.epoch != epoch {
                        let bytes = match ship.phase {
                            ShipPhase::InFlight { bytes, .. } => bytes,
                            _ => 0,
                        };
                        replenish_credit(&mut self.credit, self.credit_limit, bytes);
                        ship.epoch = epoch;
                        ship.cursor = 0;
                        ship.phase = ShipPhase::Unoffered;
                    }
                    match &ship.phase {
                        ShipPhase::Unoffered => to_offer.push((id, state)),
                        ShipPhase::Fenced { since, .. } if since.elapsed() > FENCE_RETRY => {
                            to_offer.push((id, state))
                        }
                        ShipPhase::Idle if ship.cursor < ship.head => to_ship.push(id),
                        _ => {}
                    }
                }
            }
        }
        for (id, state) in to_offer {
            let descriptor = {
                let catalog = self.ctx.catalog.clone();
                tokio::task::spawn_blocking(move || catalog.get_source(id))
                    .await
                    .context("source task")??
            };
            let Some(record) = descriptor else { continue };
            let msg = Message::Offer {
                descriptor: RemoteSourceDescriptor {
                    remote_source_id: id,
                    name: record.name,
                    kind: record.kind,
                    root_path: record.root_path,
                    aliases: record.aliases,
                },
                epoch: state.epoch.to_source_epoch(),
                head_seq: state.head_seq,
                head_chain: state.head_chain,
                compacted_through: state.compacted_through,
                image_version: SYNC_ROW_IMAGE_VERSION,
            };
            self.ctx.counters.add(&self.ctx.counters.offers_sent, 1);
            self.send(&msg).await?;
            if let Some(ship) = self.shipping.get_mut(&id) {
                ship.phase = ShipPhase::Offered {
                    since: Instant::now(),
                };
            }
        }
        for id in to_ship {
            self.ship(id).await?;
        }
        self.publish_view();
        Ok(())
    }

    fn publish_view(&self) {
        let mut sources: Vec<SessionSourceView> = self
            .shipping
            .iter()
            .map(|(id, s)| SessionSourceView {
                source_id: *id,
                role: SyncRole::Shipping,
                phase: match &s.phase {
                    ShipPhase::Unoffered => "unoffered".into(),
                    ShipPhase::Offered { .. } => "offered".into(),
                    ShipPhase::Idle => {
                        if s.cursor >= s.head {
                            "in sync".into()
                        } else {
                            "waiting for credit".into()
                        }
                    }
                    ShipPhase::InFlight { through_seq, .. } => {
                        format!("batch through {through_seq} in flight")
                    }
                    ShipPhase::Repairing { .. } => "repairing".into(),
                    ShipPhase::Fenced { reason, .. } => format!("fenced: {reason}"),
                },
                cursor: s.cursor,
                head: s.head,
                in_flight_bytes: match s.phase {
                    ShipPhase::InFlight { bytes, .. } => bytes,
                    _ => 0,
                },
                last_error: s.last_error.clone(),
                batches: s.batches,
                rows: s.rows,
            })
            .collect();
        sources.extend(self.consuming.values().map(|c| SessionSourceView {
            source_id: c.local,
            role: SyncRole::Consuming,
            phase: c.phase.clone(),
            cursor: c.applied,
            head: c.head,
            in_flight_bytes: 0,
            last_error: c.last_error.clone(),
            batches: c.batches,
            rows: c.rows,
        }));
        let mut view = self.view.lock();
        view.sources = sources;
        view.last_activity_ms_ago = self.last_rx.elapsed().as_millis() as u64;
        view.credit_remaining = self.credit;
        let _ = self.started;
        let _ = self.direction;
        let _ = self.key;
    }
}
