//! Minimize and serialize a failing simulation universe.
//!
//! Event removal uses classic `ddmin` complement reduction. Probability
//! knobs are then exhaustively lowered (their domain is only `0..=1000`).
//! The outer fixpoint matters: lowering a knob can make another event
//! removable. The final plan is one-minimal for events and numerically
//! minimal for both probability knobs under the supplied failure predicate.

use crate::sim::{FaultPlan, NodeFactory, PlanError, Simulation};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const REPLAY_SCHEMA: &str = "eidos-sync-replay/1";

/// A complete deterministic universe independent of protocol factories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    pub seed: u64,
    pub plan: FaultPlan,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReplayWire {
    schema: String,
    seed: u64,
    plan: FaultPlan,
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
        if wire.schema != REPLAY_SCHEMA {
            return Err(ReplayError::Schema(wire.schema));
        }
        Ok(Self {
            seed: wire.seed,
            plan: wire.plan,
        })
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(&ReplayWire {
            schema: REPLAY_SCHEMA.to_string(),
            seed: self.seed,
            plan: self.plan.clone(),
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

/// A one-line, valid Rust expression for logs and CI. Paste the returned
/// expression into a test and call `.simulation(factories)` on it.
pub fn reproducer(seed: u64, plan: &FaultPlan) -> String {
    let replay = Replay {
        seed,
        plan: plan.clone(),
    };
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
        };
        assert_eq!(Replay::from_json(&replay.to_json()).unwrap(), replay);
        let expression = reproducer(replay.seed, &replay.plan);
        assert!(expression.starts_with("eidos_sync::shrink::Replay::from_json("));
        assert!(expression.contains(REPLAY_SCHEMA));
    }
}
