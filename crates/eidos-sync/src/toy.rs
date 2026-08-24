//! A miniature shipper/applier pair that exists to prove the harness.
//!
//! The source appends rows (identified by a dense `0..next` sequence),
//! ships the unacknowledged suffix on a tick, and compacts what the central
//! acknowledged; the central applies idempotently, advances a contiguous
//! watermark, persists it, and only then acks. This is the real protocol's
//! skeleton (outbox → ship → same-tx watermark → ack → compact) small
//! enough to verify by eye — so when an invariant fires here, the harness
//! is what found the bug, not luck.
//!
//! `compact_before_ack` is a deliberately buggy mode that compacts as soon
//! as rows are shipped. It is indistinguishable from the correct protocol
//! on a clean network and loses rows exactly when the network drops the
//! right message — the class of bug deterministic simulation exists to
//! catch.

use crate::env::{Env, Node, NodeId};
use crate::sim::InvariantCtx;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const SOURCE: NodeId = 0;
pub const CENTRAL: NodeId = 1;
const TICK: u32 = 1;
const TICK_NS: u64 = 20_000_000;
const BATCH: u64 = 32;

#[derive(Debug, Clone)]
pub enum ToyMsg {
    Rows { rows: Vec<u64> },
    Ack { watermark: u64 },
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SourceState {
    /// Rows 0..next exist; the payload is the id itself.
    pub next: u64,
    /// Highest contiguous watermark the central has acknowledged.
    pub acked: u64,
    /// Rows below this are gone from the source. Must never pass `acked`.
    pub compacted: u64,
}

pub struct ToySource {
    /// How many rows this run should produce.
    pub target: u64,
    /// The deliberate bug: compact on ship instead of on ack.
    pub compact_before_ack: bool,
    state: SourceState,
}

impl ToySource {
    pub fn new(target: u64, compact_before_ack: bool) -> Self {
        Self {
            target,
            compact_before_ack,
            state: SourceState::default(),
        }
    }

    fn persist(&self, env: &mut dyn Env<ToyMsg>) {
        env.fs().write_durable(
            "src",
            serde_json::to_vec(&self.state).expect("serialize source state"),
        );
    }
}

impl Node for ToySource {
    type Msg = ToyMsg;

    fn on_start(&mut self, env: &mut dyn Env<ToyMsg>) {
        if let Some(bytes) = env.fs().read("src") {
            self.state = serde_json::from_slice(bytes).expect("recover source state");
        }
        env.set_timer(TICK_NS, TICK);
    }

    fn on_message(&mut self, env: &mut dyn Env<ToyMsg>, _from: NodeId, msg: ToyMsg) {
        if let ToyMsg::Ack { watermark } = msg {
            // Stale acks (duplicated or delayed) must not regress anything.
            if watermark > self.state.acked {
                self.state.acked = watermark;
                if !self.compact_before_ack {
                    self.state.compacted = self.state.acked;
                }
                self.persist(env);
            }
        }
    }

    fn on_timer(&mut self, env: &mut dyn Env<ToyMsg>, timer: u32) {
        debug_assert_eq!(timer, TICK);
        if self.state.next < self.target {
            self.state.next += 1;
            self.persist(env);
        }
        // Ship (and re-ship) the suffix the source still holds. The correct
        // protocol retains everything unacked; the buggy one has already
        // compacted shipped rows away and can only offer what is left.
        let from = self.state.compacted.max(self.state.acked);
        let until = self.state.next.min(from + BATCH);
        if from < until {
            env.send(
                CENTRAL,
                ToyMsg::Rows {
                    rows: (from..until).collect(),
                },
            );
            if self.compact_before_ack {
                self.state.compacted = until;
                self.persist(env);
            }
        }
        env.set_timer(TICK_NS, TICK);
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CentralState {
    /// Rows 0..watermark are applied; durable before any ack is sent.
    pub watermark: u64,
    /// Applied rows above the watermark (arrived past a gap).
    pub buffered: BTreeSet<u64>,
}

pub struct ToyCentral {
    /// The second deliberate bug when false: apply and ack without fsync.
    /// Invisible until a crash — the durable watermark then regresses below
    /// what was acknowledged, and rows the source compacted are gone.
    pub durable_before_ack: bool,
    state: CentralState,
}

impl ToyCentral {
    pub fn new(durable_before_ack: bool) -> Self {
        Self {
            durable_before_ack,
            state: CentralState::default(),
        }
    }
}

impl Node for ToyCentral {
    type Msg = ToyMsg;

    fn on_start(&mut self, env: &mut dyn Env<ToyMsg>) {
        if let Some(bytes) = env.fs().read("ctr") {
            self.state = serde_json::from_slice(bytes).expect("recover central state");
        }
    }

    fn on_message(&mut self, env: &mut dyn Env<ToyMsg>, from: NodeId, msg: ToyMsg) {
        if let ToyMsg::Rows { rows } = msg {
            for r in rows {
                if r >= self.state.watermark {
                    self.state.buffered.insert(r);
                }
            }
            while self.state.buffered.remove(&self.state.watermark) {
                self.state.watermark += 1;
            }
            // Durability before acknowledgement: an ack promises the source
            // it may compact, so the promise must survive a crash here.
            let bytes = serde_json::to_vec(&self.state).expect("serialize central state");
            if self.durable_before_ack {
                env.fs().write_durable("ctr", bytes);
            } else {
                env.fs().write("ctr", bytes);
            }
            env.send(
                from,
                ToyMsg::Ack {
                    watermark: self.state.watermark,
                },
            );
        }
    }

    fn on_timer(&mut self, _env: &mut dyn Env<ToyMsg>, _timer: u32) {}
}

// The toy invariants read the LIVE (buffered) storage images plus in-flight
// messages: a violation then means a row's every copy is actually gone right
// now — so a latent bug is flagged at the exact simulated fault that
// materializes it (the drop, the crash), which is what makes the seed
// search and shrinker meaningful. (A simulated crash reverts buffered state
// to the durable image, so fsync bugs surface the moment a crash lands.)
// Invariants for the real protocol may additionally reason over `durable`
// images directly.

fn source_state<M>(ctx: &InvariantCtx<'_, M>) -> SourceState {
    ctx.buffered(SOURCE, "src")
        .map(|b| serde_json::from_slice(b).expect("source state"))
        .unwrap_or_default()
}

fn central_state<M>(ctx: &InvariantCtx<'_, M>) -> CentralState {
    ctx.buffered(CENTRAL, "ctr")
        .map(|b| serde_json::from_slice(b).expect("central state"))
        .unwrap_or_default()
}

/// Safety: every appended row is held somewhere — retained by the source,
/// applied at the central, or inside an undelivered message. The network may
/// delay a row, never lose it; a state where the only copy has been dropped
/// is the violation.
pub fn no_lost_rows(ctx: &InvariantCtx<'_, ToyMsg>) -> Result<(), String> {
    let src = source_state(ctx);
    let ctr = central_state(ctx);
    for r in 0..src.next {
        let retained = r >= src.compacted;
        let applied = r < ctr.watermark || ctr.buffered.contains(&r);
        let in_flight = || {
            ctx.in_flight().any(|(_, _, m)| match m {
                ToyMsg::Rows { rows } => rows.contains(&r),
                ToyMsg::Ack { .. } => false,
            })
        };
        if !retained && !applied && !in_flight() {
            return Err(format!(
                "row {r} lost: compacted={} watermark={} buffered={:?}",
                src.compacted, ctr.watermark, ctr.buffered
            ));
        }
    }
    Ok(())
}

/// Safety: never compact above the acknowledged watermark.
pub fn compaction_below_ack(ctx: &InvariantCtx<'_, ToyMsg>) -> Result<(), String> {
    let src = source_state(ctx);
    if src.compacted > src.acked {
        return Err(format!(
            "compacted {} above acked {}",
            src.compacted, src.acked
        ));
    }
    Ok(())
}

/// Safety: the central's live watermark never regresses. A central that
/// acks before making its apply durable regresses exactly when a crash
/// discards the un-fsynced state.
pub fn watermark_monotonic() -> impl FnMut(&InvariantCtx<'_, ToyMsg>) -> Result<(), String> {
    let mut prev = 0u64;
    move |ctx| {
        let w = central_state(ctx).watermark;
        if w < prev {
            return Err(format!("central watermark regressed {prev} -> {w}"));
        }
        prev = w;
        Ok(())
    }
}
