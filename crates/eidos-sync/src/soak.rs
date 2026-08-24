//! Fast protocol scenario used by the million-seed nightly gate.

use crate::env::Node;
use crate::identity::SourceEpoch;
use crate::protocol::{
    self, Applier, CursorState, LocalMutation, MaterializedRow, ReplicaState, Shipper, SyncMsg,
    CURSOR_STATE_KEY, REPLICA_STATE_KEY,
};
use crate::rng::DeterministicRng;
use crate::shrink::reproducer;
use crate::sim::{FaultPlan, Invariant, NodeFactory, Simulation};
use eidos_domain::{ObjectId, SourceId};
use serde::Serialize;
use std::collections::BTreeMap;

const SOURCE_NODE: usize = 0;
const CENTRAL_NODE: usize = 1;
const SOURCE: SourceId = SourceId::new(1);
const HORIZON_NS: u64 = 200_000_000;
const UNTIL_NS: u64 = 600_000_000;
const MAX_STEPS: u64 = 5_000;

#[derive(Debug, Clone, Serialize)]
pub struct SoakFailure {
    pub seed: u64,
    pub message: String,
    pub reproducer: String,
}

fn epoch() -> SourceEpoch {
    SourceEpoch::random_v4(0x534f_414b, 1)
}

fn workload() -> Vec<LocalMutation> {
    let mut script = Vec::new();
    for generation in 1..=3u64 {
        for object in 1..=4i64 {
            script.push(LocalMutation {
                object: ObjectId::new(object),
                generation,
                value: Some(format!("{object}:{generation}").into_bytes()),
            });
        }
    }
    script.push(LocalMutation {
        object: ObjectId::new(4),
        generation: 4,
        value: None,
    });
    script
}

fn expected(script: &[LocalMutation]) -> BTreeMap<ObjectId, MaterializedRow> {
    script
        .iter()
        .enumerate()
        .map(|(index, mutation)| {
            (
                mutation.object,
                MaterializedRow {
                    seq: index as u64 + 1,
                    generation: mutation.generation,
                    value: mutation.value.clone(),
                },
            )
        })
        .collect()
}

pub fn run_seed(seed: u64) -> Result<(), SoakFailure> {
    let script = workload();
    let source_script = script.clone();
    let factories: Vec<NodeFactory<SyncMsg>> = vec![
        Box::new(move || {
            Box::new(
                Shipper::new(SOURCE, epoch(), CENTRAL_NODE, source_script.clone())
                    .with_repair_leaf_bits(8),
            ) as Box<dyn Node<Msg = SyncMsg>>
        }),
        Box::new(|| Box::new(Applier::new(true)) as _),
    ];
    let plan = FaultPlan::random(&mut DeterministicRng::new(seed), 2, HORIZON_NS);
    let replay = reproducer(seed, &plan);
    let fail = |message| SoakFailure {
        seed,
        message,
        reproducer: replay.clone(),
    };
    let mut sim = Simulation::new(seed, plan, factories)
        .map_err(|error| fail(format!("invalid generated plan: {error}")))?;
    let mut invariants: Vec<Invariant<'_, SyncMsg>> = vec![
        Box::new(protocol::compaction_respects_oldest_watermark(SOURCE_NODE)),
        Box::new(protocol::watermark_monotonic(CENTRAL_NODE, SOURCE)),
        Box::new(protocol::effects_do_not_lead_watermarks(CENTRAL_NODE)),
        Box::new(protocol::no_lost_tombstones(
            SOURCE_NODE,
            CENTRAL_NODE,
            SOURCE,
            vec![(script.len() as u64, ObjectId::new(4))],
        )),
    ];
    sim.run_until(UNTIL_NS, MAX_STEPS, &mut invariants)
        .map_err(|violation| fail(violation.to_string()))?;
    let replicas: ReplicaState = sim
        .durable(CENTRAL_NODE, REPLICA_STATE_KEY)
        .map(serde_json::from_slice)
        .transpose()
        .map_err(|error| fail(format!("invalid durable replica state: {error}")))?
        .unwrap_or_default();
    let cursors: CursorState = sim
        .durable(CENTRAL_NODE, CURSOR_STATE_KEY)
        .map(serde_json::from_slice)
        .transpose()
        .map_err(|error| fail(format!("invalid durable cursor state: {error}")))?
        .unwrap_or_default();
    if replicas.sources.get(&SOURCE) != Some(&expected(&script)) {
        return Err(fail(
            "replica did not converge to the materialized source image".into(),
        ));
    }
    if cursors
        .sources
        .get(&SOURCE)
        .map(|cursor| cursor.applied_seq)
        != Some(script.len() as u64)
    {
        return Err(fail(
            "replica watermark did not converge to the source head".into(),
        ));
    }
    Ok(())
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
}
