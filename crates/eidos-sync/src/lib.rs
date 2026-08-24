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
//! - [`sim`]: the seeded simulation — an event heap, a fault plan (message
//!   loss, duplication, delay, partitions, crash/restart), and invariant
//!   hooks checked after every event.
//! - [`shrink`]: delta-debugging of a failing fault plan down to a minimal
//!   versioned, pasteable reproducer.
//! - [`toy`]: a miniature shipper/applier pair (append → ship → ack →
//!   compact) that proves the harness catches the bug classes the real
//!   protocol must never have.
//!
//! Protocol state machines are added to this crate only after their behavior
//! can be expressed and checked through these deterministic seams.

pub mod env;
pub mod rng;
pub mod shrink;
pub mod sim;
pub mod toy;
