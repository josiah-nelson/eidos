//! Minimize and serialize a failing simulation universe.
//!
//! Event removal uses classic `ddmin` complement reduction. Probability
//! knobs are then exhaustively lowered (their domain is only `0..=1000`).
//! The outer fixpoint matters: lowering a knob can make another event
//! removable. The final plan is one-minimal for events and numerically
//! minimal for both probability knobs under the supplied failure predicate.

use crate::sim::{FaultPlan, NodeFactory, PlanError, Simulation};
use crate::workload::{SourceOp, WorkloadPlan};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const REPLAY_SCHEMA: &str = "eidos-sync-replay/2";
/// Still accepted: fault-only replays recorded before workloads existed.
pub const REPLAY_SCHEMA_V1: &str = "eidos-sync-replay/1";

/// A complete deterministic universe independent of protocol factories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    pub seed: u64,
    pub plan: FaultPlan,
    /// `None` for a fixed-script scenario; `Some` for generated universes.
    pub workload: Option<WorkloadPlan>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReplayWire {
    schema: String,
    seed: u64,
    plan: FaultPlan,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workload: Option<WorkloadPlan>,
}

#[derive(Debug, Error)]
pub enum ReplayError {
    #[error("invalid replay JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported replay schema {0:?}")]
    Schema(String),
}

impl Replay {
    pub fn from_json(json: &str) -> Result<Self, ReplayError> {
        let wire: ReplayWire = serde_json::from_str(json)?;
        if wire.schema != REPLAY_SCHEMA && wire.schema != REPLAY_SCHEMA_V1 {
            return Err(ReplayError::Schema(wire.schema));
        }
        Ok(Self {
            seed: wire.seed,
            plan: wire.plan,
            workload: wire.workload,
        })
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(&ReplayWire {
            schema: REPLAY_SCHEMA.to_string(),
            seed: self.seed,
            plan: self.plan.clone(),
            workload: self.workload.clone(),
        })
        .expect("replay consists only of infallibly serializable values")
    }

    pub fn simulation<M>(self, factories: Vec<NodeFactory<M>>) -> Result<Simulation<M>, PlanError>
    where
        M: Clone + std::fmt::Debug,
    {
        Simulation::new(self.seed, self.plan, factories)
    }
}

/// Shrink `plan` while `still_fails` holds. `still_fails` must re-run the
/// simulation from scratch with the candidate plan and the same seed.
pub fn shrink(mut plan: FaultPlan, still_fails: impl Fn(&FaultPlan) -> bool) -> FaultPlan {
    assert!(still_fails(&plan), "shrink requires a failing plan");
    loop {
        let before = plan.clone();
        ddmin_events(&mut plan, &still_fails);
        minimize_probabilities(&mut plan, &still_fails);
        if plan == before {
            return plan;
        }
    }
}

fn ddmin_events(plan: &mut FaultPlan, still_fails: &impl Fn(&FaultPlan) -> bool) {
    let mut granularity = 2usize;
    while plan.events.len() >= 2 {
        let len = plan.events.len();
        granularity = granularity.min(len);
        let chunk = len.div_ceil(granularity);
        let mut reduced = false;
        let mut start = 0usize;
        while start < len {
            let end = (start + chunk).min(len);
            let mut candidate = plan.clone();
            candidate.events.drain(start..end);
            if still_fails(&candidate) {
                *plan = candidate;
                granularity = granularity.saturating_sub(1).max(2);
                reduced = true;
                break;
            }
            start = end;
        }
        if !reduced {
            if granularity == len {
                break;
            }
            granularity = (granularity * 2).min(len);
        }
    }

    // Make the one-minimal contract explicit even for non-monotone fault
    // predicates, where one fault can mask another.
    let mut index = 0;
    while index < plan.events.len() {
        let mut candidate = plan.clone();
        candidate.events.remove(index);
        if still_fails(&candidate) {
            *plan = candidate;
        } else {
            index += 1;
        }
    }
}

fn minimize_probabilities(plan: &mut FaultPlan, still_fails: &impl Fn(&FaultPlan) -> bool) {
    for value in 0..plan.drop_permille {
        let mut candidate = plan.clone();
        candidate.drop_permille = value;
        if still_fails(&candidate) {
            *plan = candidate;
            break;
        }
    }
    for value in 0..plan.dup_permille {
        let mut candidate = plan.clone();
        candidate.dup_permille = value;
        if still_fails(&candidate) {
            *plan = candidate;
            break;
        }
    }
}

/// Minimize a whole universe: faults first (they are the usual cause), then
/// workload steps per source with `ddmin`, then faults again, to a fixpoint.
/// Workload candidates that no longer validate (a rewind without its
/// checkpoint, a generation regression) are skipped rather than tried.
pub fn shrink_universe(
    mut fault: FaultPlan,
    mut workload: WorkloadPlan,
    still_fails: impl Fn(&FaultPlan, &WorkloadPlan) -> bool,
) -> (FaultPlan, WorkloadPlan) {
    assert!(
        still_fails(&fault, &workload),
        "shrink requires a failing universe"
    );
    loop {
        let before = (fault.clone(), workload.clone());
        fault = shrink(fault, |candidate| still_fails(candidate, &workload));
        workload = shrink_workload(workload, |candidate| still_fails(&fault, candidate));
        if (fault.clone(), workload.clone()) == before {
            return (fault, workload);
        }
    }
}

/// `ddmin` over each source's steps, then one-minimal removal, then drop
/// sources whose steps are all gone. Only validating candidates are tried.
pub fn shrink_workload(
    mut plan: WorkloadPlan,
    still_fails: impl Fn(&WorkloadPlan) -> bool,
) -> WorkloadPlan {
    let fails = |candidate: &WorkloadPlan| candidate.validate().is_ok() && still_fails(candidate);
    for index in 0..plan.sources.len() {
        let mut granularity = 2usize;
        while plan.sources[index].ops.len() >= 2 {
            let len = plan.sources[index].ops.len();
            granularity = granularity.min(len);
            let chunk = len.div_ceil(granularity);
            let mut reduced = false;
            let mut start = 0usize;
            while start < len {
                let end = (start + chunk).min(len);
                let mut candidate = plan.clone();
                candidate.sources[index].ops.drain(start..end);
                if fails(&candidate) {
                    plan = candidate;
                    granularity = granularity.saturating_sub(1).max(2);
                    reduced = true;
                    break;
                }
                start = end;
            }
            if !reduced {
                if granularity == len {
                    break;
                }
                granularity = (granularity * 2).min(len);
            }
        }
        let mut op = 0;
        while op < plan.sources[index].ops.len() {
            let mut candidate = plan.clone();
            candidate.sources[index].ops.remove(op);
            if fails(&candidate) {
                plan = candidate;
            } else {
                op += 1;
            }
        }
        // A rewind's checkpoint is only load-bearing through the rewind;
        // once the rewind is gone the checkpoint is noise.
        let ops = &plan.sources[index].ops;
        if !ops.iter().any(|op| matches!(op, SourceOp::Rewind)) {
            let mut candidate = plan.clone();
            candidate.sources[index]
                .ops
                .retain(|op| !matches!(op, SourceOp::Checkpoint));
            if fails(&candidate) {
                plan = candidate;
            }
        }
    }
    let mut index = 0;
    while index < plan.sources.len() && plan.sources.len() > 1 {
        let mut candidate = plan.clone();
        candidate.sources.remove(index);
        if fails(&candidate) {
            plan = candidate;
        } else {
            index += 1;
        }
    }
    plan
}

/// A one-line, valid Rust expression for logs and CI. Paste the returned
/// expression into a test and call `.simulation(factories)` on it.
pub fn reproducer(seed: u64, plan: &FaultPlan) -> String {
    reproducer_universe(&Replay {
        seed,
        plan: plan.clone(),
        workload: None,
    })
}

/// [`reproducer`] for a universe that also carries a generated workload.
pub fn reproducer_universe(replay: &Replay) -> String {
    format!(
        "eidos_sync::shrink::Replay::from_json(r###\"{}\"###).expect(\"valid replay\")",
        replay.to_json()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::FaultEvent;

    fn plan() -> FaultPlan {
        FaultPlan {
            drop_permille: 9,
            dup_permille: 7,
            min_delay_ns: 1,
            max_delay_ns: 2,
            fault_horizon_ns: 10,
            events: vec![
                FaultEvent::Crash { node: 0, at_ns: 1 },
                FaultEvent::Restart { node: 0, at_ns: 2 },
                FaultEvent::Crash { node: 0, at_ns: 3 },
                FaultEvent::Restart { node: 0, at_ns: 4 },
            ],
        }
    }

    #[test]
    fn ddmin_removes_irrelevant_events_and_lowers_knobs() {
        let shrunk = shrink(plan(), |p| {
            p.events
                .iter()
                .any(|e| matches!(e, FaultEvent::Crash { at_ns: 3, .. }))
                && p.drop_permille >= 4
        });
        assert_eq!(shrunk.events, vec![FaultEvent::Crash { node: 0, at_ns: 3 }]);
        assert_eq!(shrunk.drop_permille, 4);
        assert_eq!(shrunk.dup_permille, 0);
    }

    #[test]
    fn replay_json_is_versioned_and_round_trips() {
        let replay = Replay {
            seed: 42,
            plan: plan(),
            workload: None,
        };
        assert_eq!(Replay::from_json(&replay.to_json()).unwrap(), replay);
        let expression = reproducer(replay.seed, &replay.plan);
        assert!(expression.starts_with("eidos_sync::shrink::Replay::from_json("));
        assert!(expression.contains(REPLAY_SCHEMA));
    }
}
