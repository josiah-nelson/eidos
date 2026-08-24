//! Sync core for the eidos fleet, simulation-first.
//!
//! Nothing in this crate opens a socket or touches a real disk. Every sync
//! state machine is written against the seams in [`env`] — clock, transport,
//! timers, and durable storage — and is exercised by the deterministic,
//! fault-injecting, single-threaded simulation in [`sim`] long before a real
//! transport exists. The harness came first deliberately: retrofitting
//! deterministic simulation onto a finished protocol does not work, so the
//! protocol grows up inside it.
//!
//! What lives here:
//! - [`env`]: explicit `Clock`, `Transport`, timers, and `Fs` seams, including
//!   atomic durable batches for effect + watermark commits.
//! - [`identity`]: volume-bound source identity, UUID source epochs, USN
//!   journal-id epoch bumps, and rewind/gap/replay admission fencing.
//! - [`protocol`]: materialize-at-ship outbox compaction, the shipper and
//!   central applier, same-transaction watermarks, ACK-gated retention, and
//!   socket-free Merkle repair messages.
//! - [`sim`]: the seeded simulation — an event heap, a fault plan (message
//!   loss, duplication, delay, partitions, crash/restart), and invariant
//!   hooks checked after every event.
//! - [`shrink`]: delta-debugging of a failing fault plan down to a minimal
//!   versioned, pasteable reproducer.
//! - [`merkle`]: row-level 2^17–2^20-leaf anti-entropy trees; only divergent
//!   leaves transfer when a compacted log no longer covers a cursor.
//! - [`toy`]: a miniature shipper/applier pair (append → ship → ack →
//!   compact) that proves the harness catches the bug classes the real
//!   protocol must never have.
//!
//! No code here opens a socket. Sprint 2 supplies authenticated transport and
//! real catalog/storage adapters around these state machines. The hard safety
//! rules already live here: compaction never crosses the oldest acknowledged
//! watermark, tombstones remain until that floor passes them, and an ACK is
//! emitted only after effect + watermark are durably committed together.

pub mod env;
pub mod identity;
pub mod merkle;
pub mod protocol;
pub mod rng;
pub mod shrink;
pub mod sim;
pub mod toy;
