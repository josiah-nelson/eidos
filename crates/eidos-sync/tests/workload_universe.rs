//! Generated universes and the ghost oracle: scenario-sized histories under
//! fault storms, and the hand-built rewind that the history chain fences.

use eidos_domain::{ObjectId, SourceId};
use eidos_sync::env::Node;
use eidos_sync::protocol::{
    AdmissionAlarm, Applier, CursorState, LocalMutation, ReplicaState, Shipper, SyncMsg,
    CURSOR_STATE_KEY, REPLICA_STATE_KEY,
};
use eidos_sync::rng::DeterministicRng;
use eidos_sync::sim::{FaultPlan, NodeFactory, Simulation};
use eidos_sync::soak::{run_universe, run_universe_in, universe};
use eidos_sync::workload::{GhostHistory, SourceOp, SourceWorkload, WorkloadParams, WorkloadPlan};

const HORIZON_NS: u64 = 1_500_000_000;
const UNTIL_NS: u64 = 4_000_000_000;
const MAX_STEPS: u64 = 200_000;
const SOURCE: SourceId = SourceId::new(1);

fn seeds() -> u64 {
    std::env::var("EIDOS_SYNC_UNIVERSE_SEEDS")
        .ok()
        .and_then(|value| value.replace('_', "").parse().ok())
        .unwrap_or(40)
}

fn mutate(object: i64, generation: u64) -> SourceOp {
    SourceOp::Mutate(LocalMutation {
        object: ObjectId::new(object),
        generation,
        value: Some(format!("{object}:{generation}").into_bytes()),
    })
}

/// Write three rows with a checkpoint after the first, keep writing until the
/// central has certainly acknowledged sequence 13, restore the checkpoint
/// (same epoch), then write a different history long enough to overtake 13.
fn rewind_past_watermark() -> WorkloadPlan {
    let mut ops = vec![
        mutate(1, 1),
        SourceOp::Checkpoint,
        mutate(2, 1),
        mutate(3, 1),
    ];
    for generation in 1..=10 {
        ops.push(mutate(9, generation));
    }
    ops.push(SourceOp::Rewind);
    ops.push(mutate(2, 1));
    for generation in 1..=20 {
        ops.push(mutate(7, generation));
    }
    WorkloadPlan {
        version: 1,
        sources: vec![SourceWorkload {
            source: SOURCE,
            epoch_seed: 77,
            ops,
        }],
    }
}

fn factories(workload: &WorkloadPlan, verify_chain: bool) -> Vec<NodeFactory<SyncMsg>> {
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
    factories.push(Box::new(move || {
        Box::new(Applier::new(true).with_history_chain_verification(verify_chain)) as _
    }));
    factories
}

fn durable<T: serde::de::DeserializeOwned>(sim: &Simulation<SyncMsg>, node: usize, key: &str) -> T {
    serde_json::from_slice(sim.durable(node, key).expect("durable state")).expect("valid state")
}

#[test]
fn same_epoch_rewind_that_overtakes_the_watermark_is_fenced_by_the_history_chain() {
    let workload = rewind_past_watermark();
    let mut sim = Simulation::new(11, FaultPlan::benign(), factories(&workload, true)).unwrap();
    run_universe_in(&mut sim, &workload, UNTIL_NS, MAX_STEPS)
        .expect("a fenced source leaves the central consistent");
    let cursors: CursorState = durable(&sim, 1, CURSOR_STATE_KEY);
    let replicas: ReplicaState = durable(&sim, 1, REPLICA_STATE_KEY);
    assert!(
        cursors.alarms.iter().any(|alarm| matches!(
            alarm,
            AdmissionAlarm::HistoryFork {
                applied_seq: 13,
                ..
            }
        )),
        "expected a history-fork alarm at the pre-rewind watermark, got {:?}",
        cursors.alarms
    );
    // The central kept the original history's image at 13 and never took a
    // row from the rewritten fork.
    let ghost = GhostHistory::replay(&workload.sources[0]);
    let original = &ghost.histories[0];
    let rows = replicas.sources.get(&SOURCE).unwrap();
    assert_eq!(cursors.sources[&SOURCE].applied_seq, 13);
    assert_eq!(rows, original.image(13).unwrap());
    assert!(!rows.contains_key(&ObjectId::new(7)));
}

#[test]
fn without_chain_verification_the_ghost_oracle_catches_the_merged_fork() {
    let workload = rewind_past_watermark();
    let mut sim = Simulation::new(11, FaultPlan::benign(), factories(&workload, false)).unwrap();
    let error = run_universe_in(&mut sim, &workload, UNTIL_NS, MAX_STEPS)
        .expect_err("a merged fork must be visible to the oracle");
    assert!(
        error.contains("not the image of any single history"),
        "unexpected failure: {error}"
    );
}

#[test]
fn scenario_sized_generated_universes_converge_under_fault_storms() {
    let mut failures = Vec::new();
    for seed in 0..seeds() {
        let mut rng = DeterministicRng::new(0x5ce0_0000 + seed);
        let workload = WorkloadPlan::random(&mut rng, &WorkloadParams::scenario());
        let fault = FaultPlan::random(&mut rng, workload.sources.len() + 1, HORIZON_NS);
        let mut sim = Simulation::new(seed, fault.clone(), factories(&workload, true)).unwrap();
        if let Err(error) = run_universe_in(&mut sim, &workload, UNTIL_NS, MAX_STEPS) {
            failures.push(format!("seed {seed}: {error}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn soak_universes_are_replayable_from_their_generated_plans() {
    for seed in 0..20 {
        let (fault, workload) = universe(seed);
        run_universe(seed, &fault, &workload).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
    }
}
