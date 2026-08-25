//! Generated protocol universes for the million-seed nightly gate.
//!
//! A universe is a seed, a [`FaultPlan`], and a [`WorkloadPlan`]: several
//! sources running generated histories (skewed upserts, deletes, recreates,
//! epoch changes, checkpoint/rewind forks) against one central while the
//! network drops, duplicates, delays, and partitions and processes crash.
//! Every event is followed by the protocol invariants and the ghost oracle;
//! the end state must converge for every source the central did not have to
//! fence. A failure is minimized (faults first, then workload steps) and
//! recorded as a pasteable replay.

use crate::env::Node;
use crate::protocol::{
    self, AdmissionAlarm, Applier, CursorState, ReplicaState, Shipper, SyncMsg, CURSOR_STATE_KEY,
    REPLICA_STATE_KEY,
};
use crate::rng::DeterministicRng;
use crate::shrink::{reproducer_universe, shrink_universe, Replay};
use crate::sim::{FaultPlan, Invariant, NodeFactory, Simulation};
use crate::workload::{GhostHistory, SourceOp, WorkloadParams, WorkloadPlan};
use eidos_domain::SourceId;
use serde::Serialize;

const HORIZON_NS: u64 = 200_000_000;
const UNTIL_NS: u64 = 900_000_000;
const MAX_STEPS: u64 = 12_000;

#[derive(Debug, Clone, Serialize)]
pub struct SoakFailure {
    pub seed: u64,
    pub message: String,
    /// Minimized universe as a pasteable Rust expression.
    pub reproducer: String,
}

/// Generate the universe for `seed`.
pub fn universe(seed: u64) -> (FaultPlan, WorkloadPlan) {
    let mut rng = DeterministicRng::new(seed);
    let workload = WorkloadPlan::random(&mut rng, &WorkloadParams::soak());
    let nodes = workload.sources.len() + 1;
    let fault = FaultPlan::random(&mut rng, nodes, HORIZON_NS);
    (fault, workload)
}

/// One shipper per source plus the central applier, central last.
pub fn factories(workload: &WorkloadPlan) -> Vec<NodeFactory<SyncMsg>> {
    let central = workload.sources.len();
    let mut factories: Vec<NodeFactory<SyncMsg>> = workload
        .sources
        .iter()
        .cloned()
        .map(|source| {
            Box::new(move || {
                Box::new(Shipper::new(source.clone(), central).with_repair_leaf_bits(8))
                    as Box<dyn Node<Msg = SyncMsg>>
            }) as NodeFactory<SyncMsg>
        })
        .collect();
    factories.push(Box::new(|| Box::new(Applier::new(true)) as _));
    factories
}

fn fenced(cursors: &CursorState, source: SourceId) -> bool {
    cursors.alarms.iter().any(|alarm| match alarm {
        AdmissionAlarm::SequenceRewind { source: s, .. }
        | AdmissionAlarm::HistoryFork { source: s, .. }
        | AdmissionAlarm::RetiredEpoch { source: s, .. } => *s == source,
    })
}

/// Every invariant a generated universe is checked against after each event.
pub fn invariants(workload: &WorkloadPlan) -> Vec<Invariant<'static, SyncMsg>> {
    let central = workload.sources.len();
    let mut invariants: Vec<Invariant<'static, SyncMsg>> = Vec::new();
    invariants.push(Box::new(protocol::effects_do_not_lead_watermarks(central)));
    for (node, source) in workload.sources.iter().enumerate() {
        invariants.push(Box::new(protocol::compaction_respects_oldest_watermark(
            node,
        )));
        invariants.push(Box::new(protocol::watermark_monotonic(
            central,
            source.source,
        )));
        invariants.push(Box::new(protocol::replica_matches_ghost(
            central,
            source.clone(),
        )));
        let linear = !source
            .ops
            .iter()
            .any(|op| matches!(op, SourceOp::EpochChange | SourceOp::Rewind));
        if linear {
            invariants.push(Box::new(protocol::no_lost_tombstones(
                node,
                central,
                source.source,
                workload.terminal_tombstones(source.source),
            )));
        }
    }
    invariants
}

/// Run one universe to completion and check its end state. The error is a
/// human-readable reason.
pub fn run_universe(seed: u64, fault: &FaultPlan, workload: &WorkloadPlan) -> Result<(), String> {
    let mut sim = Simulation::new(seed, fault.clone(), factories(workload))
        .map_err(|error| format!("invalid fault plan: {error}"))?;
    run_universe_in(&mut sim, workload, UNTIL_NS, MAX_STEPS)
}

/// [`run_universe`] over a caller-built simulation (custom node knobs) and
/// time/step budget. The simulation must have `workload.sources.len() + 1`
/// nodes with the central last.
pub fn run_universe_in(
    sim: &mut Simulation<SyncMsg>,
    workload: &WorkloadPlan,
    until_ns: u64,
    max_steps: u64,
) -> Result<(), String> {
    workload
        .validate()
        .map_err(|error| format!("invalid workload: {error}"))?;
    let central = workload.sources.len();
    let mut invariants = invariants(workload);
    sim.run_until(until_ns, max_steps, &mut invariants)
        .map_err(|violation| violation.to_string())?;

    let replicas: ReplicaState = sim
        .durable(central, REPLICA_STATE_KEY)
        .map(serde_json::from_slice)
        .transpose()
        .map_err(|error| format!("invalid durable replica state: {error}"))?
        .unwrap_or_default();
    let cursors: CursorState = sim
        .durable(central, CURSOR_STATE_KEY)
        .map(serde_json::from_slice)
        .transpose()
        .map_err(|error| format!("invalid durable cursor state: {error}"))?
        .unwrap_or_default();
    for source in &workload.sources {
        let id = source.source;
        let ghost = GhostHistory::replay(source);
        let Some(history) = ghost.current_history() else {
            continue;
        };
        if fenced(&cursors, id) {
            // A fenced source may not converge; the ghost invariant already
            // proved the central kept one consistent history.
            continue;
        }
        let cursor = cursors
            .sources
            .get(&id)
            .ok_or_else(|| format!("source {id}: central never admitted it"))?;
        let final_epoch = source.epoch(history.incarnation);
        if cursor.epoch != final_epoch {
            return Err(format!(
                "source {id}: central epoch {} is not the final incarnation {final_epoch}",
                cursor.epoch
            ));
        }
        if cursor.applied_seq != history.head() {
            return Err(format!(
                "source {id}: watermark {} did not converge to head {}",
                cursor.applied_seq,
                history.head()
            ));
        }
        let rows = replicas.sources.get(&id).cloned().unwrap_or_default();
        ghost
            .check_replica(|n| source.epoch(n), cursor.epoch, cursor.applied_seq, &rows)
            .map_err(|reason| format!("source {id}: {reason}"))?;
        let expected_live = history
            .images
            .last()
            .map(|image| image.values().filter(|row| row.value.is_some()).count())
            .unwrap_or(0);
        let live = rows.values().filter(|row| row.value.is_some()).count();
        if live != expected_live {
            return Err(format!(
                "source {id}: replica has {live} live rows, history has {expected_live}"
            ));
        }
    }
    Ok(())
}

/// Run the generated universe for `seed`; on failure, minimize it.
pub fn run_seed(seed: u64) -> Result<(), SoakFailure> {
    let (fault, workload) = universe(seed);
    let Err(message) = run_universe(seed, &fault, &workload) else {
        return Ok(());
    };
    // Shrink toward the protocol failure, not toward a universe the
    // simulation cannot even construct.
    let (fault, workload) = shrink_universe(fault, workload, |fault, workload| {
        run_universe(seed, fault, workload).is_err_and(|error| !error.starts_with("invalid "))
    });
    let message = run_universe(seed, &fault, &workload)
        .err()
        .unwrap_or(message);
    Err(SoakFailure {
        seed,
        message,
        reproducer: reproducer_universe(&Replay {
            seed,
            plan: fault,
            workload: Some(workload),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_hundred_soak_universes_converge() {
        for seed in 0..100 {
            run_seed(seed).unwrap_or_else(|failure| panic!("{failure:?}"));
        }
    }

    #[test]
    fn generated_universes_exercise_every_transition() {
        // Coverage, not luck: across the first seeds the generator must
        // reach multiple sources, epoch changes, and forks.
        let mut multi = false;
        let mut epochs = false;
        let mut forks = false;
        for seed in 0..300 {
            let (_, workload) = universe(seed);
            multi |= workload.sources.len() > 1;
            for op in workload.sources.iter().flat_map(|s| s.ops.iter()) {
                epochs |= matches!(op, SourceOp::EpochChange);
                forks |= matches!(op, SourceOp::Rewind);
            }
        }
        assert!(
            multi && epochs && forks,
            "multi={multi} epochs={epochs} forks={forks}"
        );
    }
}
