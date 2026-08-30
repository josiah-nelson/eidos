//! Fleet transport for eidos (ADR-0023): node identity, enrollment, and the
//! authenticated duplex session that carries the `eidos-sync` protocol
//! between a node's catalog ledger and a central's replica.
//!
//! Nothing here defines ordering or durability: those semantics live in
//! [`eidos_sync`] (verified under simulation) and in the catalog adapters
//! ([`eidos_catalog::sync`], [`eidos_catalog::replica`]). This crate
//! supplies what the simulation abstracted away - sockets, TLS, identity,
//! reconnection, duplicate-session resolution, flow control - and keeps the
//! rule that an acknowledgement follows a durable commit.
//!
//! - [`identity`]: self-signed per-node certificates, fingerprints, node
//!   ids, invitation codes.
//! - [`tls`]: mutual TLS with pinned fingerprints, no certificate authority.
//! - [`wire`]: versioned length-prefixed frames and the message set.
//! - [`session`]: one authenticated connection running both roles.
//! - [`runtime`]: listener, dialers, the session registry with its
//!   deterministic tie-break, and the maintenance loops (enable/backfill/
//!   collect on a node, aggregates on a central).
//! - [`enroll`]: the invitation exchange.
//! - [`config`], [`metrics`], [`status`]: what the service reads and shows.

pub mod bakeoff;
pub mod config;
pub mod enroll;
pub mod identity;
pub mod metrics;
pub mod runtime;
pub mod session;
pub mod status;
pub mod tls;
pub mod wire;

pub use config::FleetConfig;
pub use identity::{InviteCode, NodeIdentity};
pub use runtime::Fleet;
pub use status::FleetStatus;
